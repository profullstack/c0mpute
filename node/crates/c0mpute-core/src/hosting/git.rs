//! Gateway side of git hosting ([`crate::git`]): a repo is a hosting site,
//! so reads are plain site reads and a push is a signed site version handed
//! to this node over HTTP instead of from a publisher's own libp2p node.
//!
//! Routes (merged into [`Hosting::router`]):
//! * `GET  /git/v1/repos`                       repos this gateway knows
//! * `GET  /git/v1/repos/{owner}/{repo}`        signed record + manifest
//! * `POST /git/v1/repos/{owner}/{repo}`        push (body: [`crate::git::encode_push`])
//! * `GET  /git/{owner}/{repo}.git/{*path}`     the repo's files, for git's
//!   dumb HTTP protocol (`git clone https://<gateway>/git/<owner>/<repo>.git`)

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path as AxPath, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use c0mpute_proto::Hash;
use tracing::info;

use super::site::{BuiltSite, MAX_SITE_BYTES, Manifest, SiteManifest};
use super::{Hosting, MAX_BYTES_PER_OWNER, TARGET_REPLICAS};
use crate::git::{self, RepoDoc, RepoState};

static ACCEPT_PUSHES: AtomicBool = AtomicBool::new(true);
/// When set, only this owner key may push here (private mode).
static ONLY_OWNER: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);
/// One push at a time per node: the stale-view check and the record swap
/// must not interleave.
static PUSH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `[hosting] git_pushes = false` turns pushes off on this node (reads
/// stay); `only_owner` restricts them to one key (private mode).
pub fn set_push_policy(on: bool, only_owner: Option<String>) {
    ACCEPT_PUSHES.store(on, Ordering::Relaxed);
    if let Ok(mut o) = ONLY_OWNER.write() {
        *o = only_owner;
    }
}

fn owner_allowed(owner: &str) -> bool {
    match ONLY_OWNER.read().ok().and_then(|o| o.clone()) {
        Some(only) => only == owner,
        None => true,
    }
}

/// A refused push: HTTP status + reason.
#[derive(Debug)]
pub struct Refused(pub StatusCode, pub String);

impl Refused {
    fn bad(e: impl std::fmt::Display) -> Self {
        Refused(StatusCode::BAD_REQUEST, format!("{e:#}"))
    }
}

/// What an accepted push produced.
#[derive(Debug, serde::Serialize)]
pub struct Accepted {
    pub site_id: String,
    pub seq: u64,
    /// Live holders of the new version right now (it spreads from here).
    pub holders: usize,
}

impl Hosting {
    /// A repo's current record + manifest, when this node knows it.
    pub async fn git_state(&self, owner: &str, repo: &str) -> Option<RepoState> {
        let rec = self.record_for(&git::site_id(owner, repo)).await?;
        if rec.owner != owner || rec.name != git::site_name(repo) {
            return None;
        }
        self.git_state_of(rec).await
    }

    /// A repo's state by site id (old `c0mpute://<site id>` URLs): the
    /// record names its owner and repo, and the client checks the id.
    pub async fn git_state_by_site(&self, site_id: &str) -> Option<RepoState> {
        let rec = self.record_for(site_id).await?;
        git::repo_of_site(&rec.name)?;
        self.git_state_of(rec).await
    }

    async fn git_state_of(&self, rec: super::site::SiteRecord) -> Option<RepoState> {
        let m = self.manifest(&rec).await.ok()?;
        // A repo is a handful of files: always a one-chunk (v1) manifest.
        let Manifest::Inline(m) = &*m else {
            return None;
        };
        let manifest = String::from_utf8(m.to_bytes()).ok()?;
        let holders = self.holders(&rec.site_id).await;
        Some(RepoState {
            record: rec,
            manifest,
            holders,
        })
    }

    /// Verify and accept a push: owner signature, ref-update rules against
    /// the version this node knows, size budgets, every chunk present (sent,
    /// already local, or fetched from the current holders). Then hold it and
    /// gossip the record like a publisher would.
    pub async fn git_push(
        &self,
        owner: &str,
        repo: &str,
        body: &[u8],
    ) -> std::result::Result<Accepted, Refused> {
        if !ACCEPT_PUSHES.load(Ordering::Relaxed) || !owner_allowed(owner) {
            return Err(Refused(
                StatusCode::FORBIDDEN,
                "this gateway does not accept pushes".into(),
            ));
        }
        let (header, sent) = git::decode_push(body).map_err(Refused::bad)?;
        let rec = header.record;
        if !rec.verify() {
            return Err(Refused(
                StatusCode::FORBIDDEN,
                "record signature does not verify".into(),
            ));
        }
        if rec.owner != owner || rec.name != git::site_name(repo) {
            return Err(Refused::bad("record is for another repo"));
        }
        if Hash::of(header.manifest.as_bytes()).to_hex() != rec.manifest {
            return Err(Refused::bad("manifest does not match the record"));
        }
        let manifest =
            SiteManifest::from_bytes(header.manifest.as_bytes()).map_err(Refused::bad)?;
        // publish_site re-serializes it; the bytes must round-trip exactly.
        if manifest.to_bytes() != header.manifest.as_bytes()
            || manifest.total_bytes() != rec.size
            || manifest.name != rec.name
        {
            return Err(Refused::bad(
                "manifest is not canonical or does not match its record",
            ));
        }
        let mut sent_by_hash: HashMap<String, Vec<u8>> = HashMap::new();
        for c in sent {
            sent_by_hash.insert(Hash::of(&c).to_hex(), c);
        }
        let wanted: std::collections::HashSet<String> =
            manifest.chunk_hashes().into_iter().collect();
        if let Some(extra) = sent_by_hash.keys().find(|h| !wanted.contains(*h)) {
            return Err(Refused::bad(format!(
                "push carries chunk {extra} the manifest does not use"
            )));
        }
        let doc_bytes = file_bytes(&manifest, git::DOC_PATH, &sent_by_hash, self)
            .await
            .map_err(|e| Refused::bad(format!("ref map: {e}")))?;
        let doc = RepoDoc::from_bytes(&doc_bytes).map_err(Refused::bad)?;
        if doc.repo != repo {
            return Err(Refused::bad("ref map names another repo"));
        }

        let _guard = PUSH_LOCK.lock().await;
        let current = self.record_for(&rec.site_id).await;
        let old = match &current {
            Some(cur) => {
                let stored = self.manifest(cur).await.map_err(|e| {
                    Refused(
                        StatusCode::BAD_GATEWAY,
                        format!("current version unavailable: {e}"),
                    )
                })?;
                let Manifest::Inline(m) = &*stored else {
                    return Err(Refused(
                        StatusCode::CONFLICT,
                        "a non-git site already has this name".into(),
                    ));
                };
                let m = m.clone();
                let d = match self.read_file(cur, git::DOC_PATH).await {
                    Ok(Some((_, b))) => RepoDoc::from_bytes(&b).map_err(|e| {
                        Refused(StatusCode::CONFLICT, format!("current version: {e}"))
                    })?,
                    // The old bundle layout: the push replaces it (a full
                    // pack, checked by the client); refs.json is the ref map.
                    Ok(None) if git::is_legacy(&m) => {
                        match self.read_file(cur, git::LEGACY_REFS).await {
                            Ok(Some((_, b))) => RepoDoc::from_legacy(repo, &b).map_err(|e| {
                                Refused(StatusCode::CONFLICT, format!("current version: {e}"))
                            })?,
                            Ok(None) => {
                                return Err(Refused(
                                    StatusCode::CONFLICT,
                                    "current version has no refs.json".into(),
                                ));
                            }
                            Err(e) => {
                                return Err(Refused(
                                    StatusCode::BAD_GATEWAY,
                                    format!("current ref map unavailable: {e}"),
                                ));
                            }
                        }
                    }
                    Ok(None) => {
                        return Err(Refused(
                            StatusCode::CONFLICT,
                            "a non-git site already has this name".into(),
                        ));
                    }
                    Err(e) => {
                        return Err(Refused(
                            StatusCode::BAD_GATEWAY,
                            format!("current ref map unavailable: {e}"),
                        ));
                    }
                };
                Some((cur.clone(), m, d))
            }
            None => None,
        };
        git::check_update(
            old.as_ref().map(|(r, m, d)| (r, m, d)),
            &rec,
            &manifest,
            &doc,
        )
        .map_err(|e| Refused(StatusCode::CONFLICT, format!("{e:#}")))?;

        // Budgets, as a hosting node applies them to any site it holds.
        let (held, owner_held) = {
            let cat = self.0.catalog.read().await;
            let others = cat.sites.values().filter(|r| r.site_id != rec.site_id);
            let (mut all, mut mine) = (0u64, 0u64);
            for r in others {
                all += r.size;
                if r.owner == rec.owner {
                    mine += r.size;
                }
            }
            (all, mine)
        };
        if rec.size > MAX_SITE_BYTES
            || held + rec.size > self.0.max_bytes
            || owner_held + rec.size > MAX_BYTES_PER_OWNER
        {
            return Err(Refused(
                StatusCode::INSUFFICIENT_STORAGE,
                "over this gateway's hosting budget".into(),
            ));
        }

        // Every chunk: sent, already here, or fetched from the current holders.
        let holders = self.0.map.read().await.sources(&rec.site_id);
        let mut chunks = Vec::new();
        for h in &wanted {
            if let Some(c) = sent_by_hash.remove(h) {
                chunks.push(c);
                continue;
            }
            let hash = Hash::from_hex(h).map_err(|_| Refused::bad("bad chunk hash"))?;
            if self.0.store.has(&hash).await {
                continue;
            }
            let bytes = self.fetch_chunk(&hash, &holders).await.map_err(|e| {
                Refused(
                    StatusCode::BAD_GATEWAY,
                    format!("chunk {h} is neither in the push nor reachable: {e}"),
                )
            })?;
            chunks.push(bytes);
        }
        let built = BuiltSite { manifest, chunks };
        self.publish_site(&built, rec.clone())
            .await
            .map_err(|e| Refused(StatusCode::CONFLICT, format!("{e:#}")))?;
        if let Some((old_rec, _, _)) = &old {
            let keep = Manifest::Inline(built.manifest.clone());
            self.collect_garbage(old_rec, Some(&keep)).await;
        }
        info!(site = %rec.site_id, repo = %repo, seq = rec.seq, refs = doc.refs.len(), packs = doc.packs.len(), "git: accepted a push");
        let holders = self.holders(&rec.site_id).await.len();
        Ok(Accepted {
            site_id: rec.site_id,
            seq: rec.seq,
            holders,
        })
    }
}

/// A file of a pushed manifest, from the pushed chunks or the local store.
async fn file_bytes(
    m: &SiteManifest,
    path: &str,
    sent: &HashMap<String, Vec<u8>>,
    h: &Hosting,
) -> Result<Vec<u8>> {
    let entry = m.files.get(path).with_context(|| format!("no {path}"))?;
    let mut out = Vec::with_capacity(entry.size as usize);
    for c in &entry.chunks {
        match sent.get(c) {
            Some(b) => out.extend_from_slice(b),
            None => {
                let hash = Hash::from_hex(c).map_err(|_| anyhow!("bad hash"))?;
                out.extend_from_slice(&h.0.store.get(&hash).await.context("chunk not sent")?);
            }
        }
    }
    if out.len() as u64 != entry.size {
        bail!("{path} has the wrong size");
    }
    Ok(out)
}

/// The git routes, merged into the hosting router.
pub fn router() -> Router<Hosting> {
    Router::new()
        .route("/git/v1/repos", get(list_repos))
        .route(
            "/git/v1/repos/{owner}/{repo}",
            get(get_repo).post(post_push).layer(DefaultBodyLimit::max(
                MAX_SITE_BYTES as usize + 64 * 1024 * 1024,
            )),
        )
        .route("/git/v1/sites/{site_id}", get(get_repo_by_site))
        .route("/git/{owner}/{repo}/{*path}", get(get_file))
}

async fn get_repo_by_site(State(h): State<Hosting>, AxPath(site_id): AxPath<String>) -> Response {
    let site_id = site_id.to_ascii_lowercase();
    if !git::valid_site_id(&site_id) {
        return (
            StatusCode::BAD_REQUEST,
            "expected /git/v1/sites/<20-hex site id>",
        )
            .into_response();
    }
    match h.git_state_by_site(&site_id).await {
        Some(s) => axum::Json(s).into_response(),
        None => (StatusCode::NOT_FOUND, "no such repo on the network (yet)").into_response(),
    }
}

fn parse(owner: &str, repo: &str) -> Option<(String, String)> {
    let owner = owner.to_ascii_lowercase();
    let repo = git::strip_dot_git(repo).to_string();
    (git::valid_owner(&owner) && git::valid_repo(&repo)).then_some((owner, repo))
}

async fn list_repos(State(h): State<Hosting>) -> Response {
    let repos: Vec<serde_json::Value> = h
        .sites()
        .await
        .into_iter()
        .filter_map(|(r, copies)| {
            let repo = git::repo_of_site(&r.name)?.to_string();
            Some(serde_json::json!({
                "owner": r.owner,
                "repo": repo,
                "url": git::url_for(&r.owner, &repo),
                "clone_path": format!("/git/{}/{}.git", r.owner, repo),
                "site_id": r.site_id,
                "seq": r.seq,
                "size": r.size,
                "copies": copies,
            }))
        })
        .collect();
    axum::Json(serde_json::json!({ "target_copies": TARGET_REPLICAS, "repos": repos }))
        .into_response()
}

async fn get_repo(
    State(h): State<Hosting>,
    AxPath((owner, repo)): AxPath<(String, String)>,
) -> Response {
    let Some((owner, repo)) = parse(&owner, &repo) else {
        return (
            StatusCode::BAD_REQUEST,
            "expected /git/v1/repos/<64-hex owner>/<repo>",
        )
            .into_response();
    };
    match h.git_state(&owner, &repo).await {
        Some(s) => axum::Json(s).into_response(),
        None => (StatusCode::NOT_FOUND, "no such repo on the network (yet)").into_response(),
    }
}

async fn post_push(
    State(h): State<Hosting>,
    AxPath((owner, repo)): AxPath<(String, String)>,
    body: Bytes,
) -> Response {
    let Some((owner, repo)) = parse(&owner, &repo) else {
        return (
            StatusCode::BAD_REQUEST,
            "expected /git/v1/repos/<64-hex owner>/<repo>",
        )
            .into_response();
    };
    match h.git_push(&owner, &repo, &body).await {
        Ok(a) => axum::Json(a).into_response(),
        Err(Refused(code, why)) => (code, why).into_response(),
    }
}

async fn get_file(
    State(h): State<Hosting>,
    AxPath((owner, repo, path)): AxPath<(String, String, String)>,
) -> Response {
    let Some((owner, repo)) = parse(&owner, &repo) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Some(rec) = h.record_for(&git::site_id(&owner, &repo)).await else {
        return (StatusCode::NOT_FOUND, "no such repo on the network (yet)").into_response();
    };
    let mut read = h.read_file(&rec, &path).await;
    if read.is_err() {
        // Right after a push the record can arrive here before any holder's
        // announcement of the new version; give the announcements a moment.
        for _ in 0..12 {
            if !h.0.map.read().await.sources(&rec.site_id).is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        read = h.read_file(&rec, &path).await;
    }
    match read {
        Ok(Some((entry, bytes))) => {
            let mut resp = (StatusCode::OK, Body::from(bytes)).into_response();
            let hdr = resp.headers_mut();
            if let Ok(v) = HeaderValue::from_str(&entry.content_type) {
                hdr.insert(header::CONTENT_TYPE, v);
            }
            // Packs are immutable (named by content); refs change on push.
            let cache = if path.starts_with("objects/pack/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            hdr.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
            resp
        }
        // Loose objects git probes for over dumb HTTP: never stored.
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("repo is unavailable right now: {e}"),
        )
            .into_response(),
    }
}
