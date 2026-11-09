//! A read-only web view of git repos on every gateway, from the stored packs
//! (no `git` binary): `/git/<owner>/<repo>` and below.
//!
//! * `/git/<owner>/<repo>`                      home: files at HEAD + README
//! * `/git/<owner>/<repo>/tree/<ref>/<path>`    a directory
//! * `/git/<owner>/<repo>/blob/<ref>/<path>`    a file
//! * `/git/<owner>/<repo>/raw/<ref>/<path>`     a file's bytes (sandboxed)
//! * `/git/<owner>/<repo>/commits/<ref>`        history (first parent)
//! * `/git/<owner>/<repo>/commit/<oid>`         one commit and its changes
//!
//! `<ref>` is a branch, a tag or a commit id. `<repo>.git/...` stays git's
//! dumb HTTP protocol; `<repo>.git` alone redirects here, so the clone URL is
//! also the link to the page. Pages carry no script and a strict CSP; raw
//! files are served sandboxed so a repo cannot script the gateway's origin.
//! An end-to-end encrypted repo cannot be read by a gateway: its page says so
//! and shows how to clone it with the key.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};

use super::Hosting;
use super::TARGET_REPLICAS;
use super::site::{Manifest, SiteRecord};
use crate::git::pack::{self, Commit, Kind, PackSet, TreeEntry};
use crate::git::{self, RepoDoc};

/// Repos whose packs add up to more than this are not browsed on a gateway
/// (they still clone).
const BROWSE_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// Parsed repos kept in memory, by manifest hash.
const CACHE_BYTES: usize = 512 * 1024 * 1024;
/// Largest file shown inline as text.
const SHOW_MAX_BYTES: usize = 2 * 1024 * 1024;
const LOG_LIMIT: usize = 100;
const DIFF_LIMIT: usize = 500;

static CACHE: Mutex<Vec<(String, Arc<PackSet>)>> = Mutex::new(Vec::new());

fn cached(manifest: &str) -> Option<Arc<PackSet>> {
    let c = CACHE.lock().ok()?;
    c.iter()
        .find(|(m, _)| m == manifest)
        .map(|(_, p)| p.clone())
}

fn remember(manifest: &str, p: Arc<PackSet>) {
    let Ok(mut c) = CACHE.lock() else { return };
    c.retain(|(m, _)| m != manifest);
    c.insert(0, (manifest.to_string(), p));
    let mut total = 0;
    c.retain(|(_, p)| {
        total += p.bytes();
        total <= CACHE_BYTES
    });
}

/// One repo, ready to show.
struct Repo {
    owner: String,
    repo: String,
    rec: SiteRecord,
    doc: RepoDoc,
    copies: usize,
    packs: Arc<PackSet>,
    /// `scheme://host` the request came in on, for clone commands.
    base: String,
}

impl Repo {
    fn url(&self, rest: &str) -> String {
        let r = format!("/git/{}/{}", self.owner, self.repo);
        if rest.is_empty() {
            r
        } else {
            format!("{r}/{rest}")
        }
    }

    /// `(display name, commit id)` of HEAD.
    fn head(&self) -> Option<(String, String)> {
        let h = self.doc.head.as_ref()?;
        let oid = self.doc.refs.get(h)?;
        let (c, _, _) = self.packs.peel(oid).ok()?;
        Some((short_ref(h).to_string(), c))
    }
}

fn short_ref(r: &str) -> &str {
    r.strip_prefix("refs/heads/")
        .or_else(|| r.strip_prefix("refs/tags/"))
        .unwrap_or(r)
}

/// `scheme://host` of the request (behind nginx: `X-Forwarded-Proto`).
fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .filter(|h| {
            h.chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c))
        })
        .unwrap_or("127.0.0.1:7777");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|p| *p == "https" || *p == "http")
        .unwrap_or(
            if host.starts_with("127.") || host.starts_with("localhost") {
                "http"
            } else {
                "https"
            },
        );
    format!("{proto}://{host}")
}

/// `/git/<owner>/<repo>`: the repo's home page; `<repo>.git` redirects.
pub async fn home(h: &Hosting, owner: &str, repo_seg: &str, headers: &HeaderMap) -> Response {
    if repo_seg.ends_with(".git") {
        return Redirect::to(&format!("/git/{owner}/{}", git::strip_dot_git(repo_seg)))
            .into_response();
    }
    match load(h, owner, repo_seg, headers).await {
        Ok(r) => tree_page(&r, None, ""),
        Err(resp) => resp,
    }
}

/// A browse path under a repo (`tree/...`, `blob/...`, ...), or `None` when
/// `path` is not one (then it is a dumb-HTTP file).
pub async fn page(
    h: &Hosting,
    owner: &str,
    repo: &str,
    path: &str,
    headers: &HeaderMap,
) -> Option<Response> {
    let (verb, rest) = path.split_once('/').unwrap_or((path, ""));
    if !matches!(verb, "tree" | "blob" | "raw" | "commits" | "commit") {
        return None;
    }
    let r = match load(h, owner, repo, headers).await {
        Ok(r) => r,
        Err(resp) => return Some(resp),
    };
    let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    Some(match verb {
        "commit" => commit_page(&r, segs.first().copied().unwrap_or("")),
        _ => {
            let Some((name, oid, used)) = resolve(&r, &segs) else {
                return Some(not_found(&r, "no such branch, tag or commit"));
            };
            let sub = segs[used..].join("/");
            match verb {
                "tree" => tree_page(&r, Some((&name, &oid)), &sub),
                "blob" => blob_page(&r, &name, &oid, &sub),
                "raw" => raw_file(&r, &oid, &sub),
                _ => commits_page(&r, &name, &oid),
            }
        }
    })
}

/// `<ref>/<path...>`: the longest prefix naming a branch or tag, or a commit
/// id. Returns `(display name, commit id, segments used by the ref)`.
fn resolve(r: &Repo, segs: &[&str]) -> Option<(String, String, usize)> {
    if segs.is_empty() {
        let (n, c) = r.head()?;
        return Some((n, c, 0));
    }
    for i in (1..=segs.len()).rev() {
        let cand = segs[..i].join("/");
        for full in [
            format!("refs/heads/{cand}"),
            format!("refs/tags/{cand}"),
            cand.clone(),
        ] {
            if let Some(oid) = r.doc.refs.get(&full)
                && let Ok((c, Kind::Commit, _)) = r.packs.peel(oid)
            {
                return Some((cand, c, i));
            }
        }
    }
    let first = segs[0].to_ascii_lowercase();
    if git::valid_oid(&first)
        && let Ok((c, Kind::Commit, _)) = r.packs.peel(&first)
    {
        // Keep the full id: links built from the name must resolve again.
        return Some((first, c, 1));
    }
    None
}

#[allow(clippy::result_large_err)] // the Err is the page to send
async fn load(h: &Hosting, owner: &str, repo: &str, headers: &HeaderMap) -> Result<Repo, Response> {
    let owner = owner.to_ascii_lowercase();
    let repo = git::strip_dot_git(repo).to_string();
    if !git::valid_owner(&owner) || !git::valid_repo(&repo) {
        return Err(message(
            StatusCode::NOT_FOUND,
            "Not found",
            "<p>Not a repo address.</p>",
        ));
    }
    let site_id = git::site_id(&owner, &repo);
    let Some(rec) = h.record_for(&site_id).await else {
        return Err(message(
            StatusCode::NOT_FOUND,
            "No such repo",
            &format!(
                "<p>This gateway has not heard of <code>{}</code> (yet). New repos reach every gateway by gossip within a few minutes.</p>",
                esc(&git::url_for(&owner, &repo))
            ),
        ));
    };
    if rec.owner != owner || rec.name != git::site_name(&repo) {
        return Err(message(StatusCode::NOT_FOUND, "No such repo", ""));
    }
    let unavailable = |e: anyhow::Error| {
        message(
            StatusCode::BAD_GATEWAY,
            "Repo unavailable",
            &format!(
                "<p>No holder of this repo answered right now: {}</p><p>Copies heal automatically; try again in a minute.</p>",
                esc(&format!("{e:#}"))
            ),
        )
    };
    let manifest = h.manifest(&rec).await.map_err(unavailable)?;
    let Manifest::Inline(m) = &*manifest else {
        return Err(message(StatusCode::NOT_FOUND, "Not a repo", ""));
    };
    let base = base_url(headers);
    let copies = h.holders(&site_id).await.len();
    if git::is_legacy(m) {
        return Err(message(
            StatusCode::OK,
            &repo,
            &format!(
                "<p>This repo is in the first c0mpute git layout (one bundle), which the web view does not read. Its owner's next <code>git push</code> converts it. It still clones:</p><pre>git clone {}</pre>",
                esc(&git::url_for(&owner, &repo))
            ),
        ));
    }
    let doc = match h.read_file(&rec, git::DOC_PATH).await {
        Ok(Some((_, b))) => RepoDoc::from_bytes(&b).map_err(&unavailable)?,
        Ok(None) => return Err(message(StatusCode::NOT_FOUND, "Not a repo", "")),
        Err(e) => return Err(unavailable(e)),
    };
    if doc.sealed.is_some() {
        return Err(e2e_page(&owner, &repo, &rec, copies));
    }
    let packs = match cached(&rec.manifest) {
        Some(p) => p,
        None => {
            let total: u64 = doc
                .packs
                .iter()
                .flat_map(|p| [git::pack_path(p), git::idx_path(p)])
                .filter_map(|f| m.files.get(&f).map(|e| e.size))
                .sum();
            if total > BROWSE_MAX_BYTES {
                return Err(message(
                    StatusCode::OK,
                    &repo,
                    &format!(
                        "<p>This repo is {} MiB, too large to browse on a gateway. Clone it:</p><pre>git clone {}/git/{owner}/{repo}.git</pre>",
                        total >> 20,
                        esc(&base)
                    ),
                ));
            }
            let mut files = Vec::new();
            for p in &doc.packs {
                let pack = h.read_file(&rec, &git::pack_path(p)).await;
                let idx = h.read_file(&rec, &git::idx_path(p)).await;
                match (pack, idx) {
                    (Ok(Some((_, pk))), Ok(Some((_, ix)))) => files.push((pk, ix)),
                    (Err(e), _) | (_, Err(e)) => return Err(unavailable(e)),
                    _ => return Err(unavailable(anyhow::anyhow!("pack {p} is missing"))),
                }
            }
            let set = Arc::new(PackSet::new(files).map_err(unavailable)?);
            remember(&rec.manifest, set.clone());
            set
        }
    };
    Ok(Repo {
        owner,
        repo,
        rec,
        doc,
        copies,
        packs,
        base,
    })
}

// ── pages ─────────────────────────────────────────────────────────────────

fn header_html(r: &Repo, at: Option<&str>) -> String {
    let mut s = String::new();
    let short = &r.owner[..12];
    let _ = write!(
        s,
        r#"<header><h1><a href="{home}">{repo}</a></h1><p class="meta"><span title="{owner}">owner {short}…</span> · {copies}/{target} copies · {size} · updated {when}</p>"#,
        home = r.url(""),
        repo = esc(&r.repo),
        owner = r.owner,
        copies = r.copies,
        target = TARGET_REPLICAS,
        size = human_bytes(r.rec.size),
        when = ago(r.doc.pushed_at / 1000),
    );
    // Branches and tags.
    let mut refs = String::new();
    for name in r.doc.refs.keys() {
        let (label, cls) = if let Some(b) = name.strip_prefix("refs/heads/") {
            (b, "branch")
        } else if let Some(t) = name.strip_prefix("refs/tags/") {
            (t, "tag")
        } else {
            continue;
        };
        let current = at == Some(label);
        let _ = write!(
            refs,
            r#"<a class="ref {cls}{cur}" href="{href}">{label}</a> "#,
            cur = if current { " current" } else { "" },
            href = r.url(&format!("tree/{}", enc_path(label))),
            label = esc(label),
        );
    }
    if !refs.is_empty() {
        let _ = write!(s, r#"<nav class="refs">{refs}</nav>"#);
    }
    s.push_str("</header>");
    s
}

fn clone_box(r: &Repo) -> String {
    format!(
        r#"<details class="clone"><summary>Clone</summary><pre>git clone {base}/git/{o}/{repo}.git
git clone {url}</pre><p>The first needs nothing but git. The second verifies the owner's signature on every file (<code>c0mpute git install</code> once).</p></details>"#,
        base = esc(&r.base),
        o = r.owner,
        repo = esc(&r.repo),
        url = esc(&git::url_for(&r.owner, &r.repo)),
    )
}

/// Breadcrumbs: repo / a / b / c, each a tree link at `at`.
fn crumbs(r: &Repo, at: &str, path: &str) -> String {
    let mut s = format!(
        r#"<p class="crumbs"><a href="{}">{}</a>"#,
        r.url(&format!("tree/{}", enc_path(at))),
        esc(&r.repo)
    );
    let mut acc = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        let _ = write!(
            s,
            r#" / <a href="{}">{}</a>"#,
            r.url(&format!("tree/{}/{}", enc_path(at), enc_path(&acc))),
            esc(part)
        );
    }
    s.push_str("</p>");
    s
}

fn tree_page(r: &Repo, at: Option<(&str, &str)>, path: &str) -> Response {
    let (name, commit) = match at {
        Some((n, c)) => (n.to_string(), c.to_string()),
        None => match r.head() {
            Some(h) => h,
            None => {
                let body = format!(
                    "{}{}<p>This repo has no commits yet.</p>",
                    header_html(r, None),
                    clone_box(r)
                );
                return html(StatusCode::OK, &r.repo, &body);
            }
        },
    };
    let c = match r.packs.object(&commit).and_then(|(_, d)| Commit::parse(&d)) {
        Ok(c) => c,
        Err(e) => return not_found(r, &format!("{e:#}")),
    };
    let entries = match pack::walk(&r.packs, &c.tree, path) {
        Ok((oid, Kind::Tree)) => match r.packs.object(&oid).and_then(|(_, d)| pack::parse_tree(&d))
        {
            Ok(e) => e,
            Err(e) => return not_found(r, &format!("{e:#}")),
        },
        Ok(_) => {
            return Redirect::to(&r.url(&format!("blob/{}/{}", enc_path(&name), enc_path(path))))
                .into_response();
        }
        Err(e) => return not_found(r, &format!("{e:#}")),
    };
    let mut body = header_html(r, Some(&name));
    body.push_str(&clone_box(r));
    let _ = write!(
        body,
        r#"<p class="last"><a href="{}">{}</a> {} <span class="dim">· {} · {}</span> · <a href="{}">history</a></p>"#,
        r.url(&format!("commit/{commit}")),
        &commit[..10],
        esc(c.subject()),
        esc(&c.author.name),
        ago(c.committer.time.max(0) as u64),
        r.url(&format!("commits/{}", enc_path(&name))),
    );
    body.push_str(&crumbs(r, &name, path));
    body.push_str(r#"<table class="files">"#);
    if !path.is_empty() {
        let parent = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let _ = write!(
            body,
            r#"<tr><td class="ico">↩</td><td><a href="{}">..</a></td></tr>"#,
            r.url(&format!("tree/{}/{}", enc_path(&name), enc_path(parent)))
        );
    }
    for e in &entries {
        let full = if path.is_empty() {
            e.name.clone()
        } else {
            format!("{path}/{}", e.name)
        };
        let (ico, href) = if e.is_dir() {
            (
                "📁",
                r.url(&format!("tree/{}/{}", enc_path(&name), enc_path(&full))),
            )
        } else if e.is_submodule() {
            ("📦", String::new())
        } else {
            (
                if e.is_symlink() { "🔗" } else { "📄" },
                r.url(&format!("blob/{}/{}", enc_path(&name), enc_path(&full))),
            )
        };
        if href.is_empty() {
            let _ = write!(
                body,
                r#"<tr><td class="ico">{ico}</td><td>{} <span class="dim">submodule @ {}</span></td></tr>"#,
                esc(&e.name),
                &e.oid[..10]
            );
        } else {
            let _ = write!(
                body,
                r#"<tr><td class="ico">{ico}</td><td><a href="{href}">{}</a></td></tr>"#,
                esc(&e.name)
            );
        }
    }
    body.push_str("</table>");
    if let Some(readme) = find_readme(&entries)
        && let Ok((Kind::Blob, data)) = r.packs.object(&readme.oid)
        && data.len() <= SHOW_MAX_BYTES
        && !is_binary(&data)
    {
        let _ = write!(
            body,
            r#"<section class="readme"><h2>{}</h2><pre class="wrap">{}</pre></section>"#,
            esc(&readme.name),
            esc(&String::from_utf8_lossy(&data))
        );
    }
    let title = if path.is_empty() {
        r.repo.clone()
    } else {
        format!("{path} · {}", r.repo)
    };
    html(StatusCode::OK, &title, &body)
}

fn find_readme(entries: &[TreeEntry]) -> Option<&TreeEntry> {
    let rank = |n: &str| {
        let l = n.to_ascii_lowercase();
        match l.as_str() {
            "readme.md" => 0,
            "readme" => 1,
            "readme.txt" => 2,
            _ if l.starts_with("readme") => 3,
            _ => 99,
        }
    };
    entries
        .iter()
        .filter(|e| !e.is_dir() && !e.is_submodule() && rank(&e.name) < 99)
        .min_by_key(|e| rank(&e.name))
}

fn blob_page(r: &Repo, name: &str, commit: &str, path: &str) -> Response {
    let data = match blob_at(r, commit, path) {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    let raw = r.url(&format!("raw/{}/{}", enc_path(name), enc_path(path)));
    let mut body = header_html(r, Some(name));
    body.push_str(&crumbs(r, name, path));
    let _ = write!(
        body,
        r#"<p class="dim">{} · <a href="{raw}">raw</a> · <a href="{}">history</a></p>"#,
        human_bytes(data.len() as u64),
        r.url(&format!("commits/{}", enc_path(name))),
    );
    if is_binary(&data) {
        if image_type(path).is_some() {
            let _ = write!(
                body,
                r#"<p><img class="img" src="{raw}" alt="{}"></p>"#,
                esc(path)
            );
        } else {
            body.push_str("<p>Binary file, not shown.</p>");
        }
    } else if data.len() > SHOW_MAX_BYTES {
        body.push_str("<p>Large file, not shown: open it raw.</p>");
    } else {
        body.push_str(r#"<table class="code">"#);
        for (i, line) in String::from_utf8_lossy(&data).lines().enumerate() {
            let n = i + 1;
            let _ = write!(
                body,
                r##"<tr id="L{n}"><td class="ln"><a href="#L{n}">{n}</a></td><td><pre>{}</pre></td></tr>"##,
                esc(line)
            );
        }
        body.push_str("</table>");
    }
    html(StatusCode::OK, &format!("{path} · {}", r.repo), &body)
}

#[allow(clippy::result_large_err)] // the Err is the page to send
fn blob_at(r: &Repo, commit: &str, path: &str) -> Result<Vec<u8>, Response> {
    let c = r
        .packs
        .object(commit)
        .and_then(|(_, d)| Commit::parse(&d))
        .map_err(|e| not_found(r, &format!("{e:#}")))?;
    match pack::walk(&r.packs, &c.tree, path) {
        Ok((oid, Kind::Blob)) => r
            .packs
            .object(&oid)
            .map(|(_, d)| d)
            .map_err(|e| not_found(r, &format!("{e:#}"))),
        Ok(_) => Err(not_found(r, "that is a directory")),
        Err(e) => Err(not_found(r, &format!("{e:#}"))),
    }
}

/// A file's bytes. Never served as HTML: text is text/plain, images keep
/// their type, and every response is sandboxed.
fn raw_file(r: &Repo, commit: &str, path: &str) -> Response {
    let data = match blob_at(r, commit, path) {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    let ct = match image_type(path) {
        Some(t) => t,
        None if !is_binary(&data) => "text/plain; charset=utf-8",
        None => "application/octet-stream",
    };
    let mut resp = (StatusCode::OK, Body::from(data)).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; sandbox",
        ),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=60"),
    );
    resp
}

fn commits_page(r: &Repo, name: &str, commit: &str) -> Response {
    let list = match pack::log(&r.packs, commit, LOG_LIMIT) {
        Ok(l) => l,
        Err(e) => return not_found(r, &format!("{e:#}")),
    };
    let mut body = header_html(r, Some(name));
    let _ = write!(body, "<h2>History of {}</h2><ol class=\"log\">", esc(name));
    for (oid, c) in &list {
        let _ = write!(
            body,
            r#"<li><a class="sha" href="{}">{}</a> {} <span class="dim">· {} · {}</span></li>"#,
            r.url(&format!("commit/{oid}")),
            &oid[..10],
            esc(c.subject()),
            esc(&c.author.name),
            ago(c.committer.time.max(0) as u64),
        );
    }
    body.push_str("</ol>");
    if list.len() == LOG_LIMIT {
        let _ = write!(body, "<p class=\"dim\">Showing the latest {LOG_LIMIT}.</p>");
    }
    html(StatusCode::OK, &format!("History · {}", r.repo), &body)
}

fn commit_page(r: &Repo, oid: &str) -> Response {
    let oid = oid.to_ascii_lowercase();
    let c = match r.packs.peel(&oid) {
        Ok((oid2, Kind::Commit, d)) if oid2 == oid => match Commit::parse(&d) {
            Ok(c) => c,
            Err(e) => return not_found(r, &format!("{e:#}")),
        },
        _ => return not_found(r, "no such commit"),
    };
    let mut body = header_html(r, None);
    let _ = write!(
        body,
        r#"<h2>{}</h2><p class="dim">{} &lt;{}&gt; · {} · commit <code>{oid}</code></p>"#,
        esc(c.subject()),
        esc(&c.author.name),
        esc(&c.author.email),
        ago(c.author.time.max(0) as u64),
    );
    let rest = c.message.split_once('\n').map(|x| x.1).unwrap_or("").trim();
    if !rest.is_empty() {
        let _ = write!(body, r#"<pre class="wrap">{}</pre>"#, esc(rest));
    }
    let _ = write!(
        body,
        r#"<p><a href="{}">Browse files at this commit</a>"#,
        r.url(&format!("tree/{oid}"))
    );
    for p in &c.parents {
        let _ = write!(
            body,
            r#" · parent <a class="sha" href="{}">{}</a>"#,
            r.url(&format!("commit/{p}")),
            &p[..10.min(p.len())]
        );
    }
    body.push_str("</p>");
    // Changed paths against the first parent.
    let old_tree = c
        .parents
        .first()
        .and_then(|p| r.packs.object(p).ok())
        .and_then(|(_, d)| Commit::parse(&d).ok())
        .map(|p| p.tree);
    let mut changes = Vec::new();
    diff_trees(
        &r.packs,
        old_tree.as_deref(),
        Some(&c.tree),
        "",
        &mut changes,
    );
    let _ = write!(
        body,
        "<h3>{} changed</h3><ul class=\"changes\">",
        if changes.len() >= DIFF_LIMIT {
            format!("{DIFF_LIMIT}+ paths")
        } else {
            format!("{} path(s)", changes.len())
        }
    );
    for (st, path) in &changes {
        let link = if *st == 'D' {
            esc(path)
        } else {
            format!(
                r#"<a href="{}">{}</a>"#,
                r.url(&format!("blob/{oid}/{}", enc_path(path))),
                esc(path)
            )
        };
        let _ = write!(
            body,
            r#"<li><span class="st st{st}">{st}</span> {link}</li>"#
        );
    }
    body.push_str("</ul>");
    html(
        StatusCode::OK,
        &format!("{} · {}", c.subject(), r.repo),
        &body,
    )
}

/// Paths added (A), modified (M) or deleted (D) between two trees.
fn diff_trees(
    p: &PackSet,
    old: Option<&str>,
    new: Option<&str>,
    prefix: &str,
    out: &mut Vec<(char, String)>,
) {
    if out.len() >= DIFF_LIMIT || old == new {
        return;
    }
    let read = |t: Option<&str>| -> BTreeMap<String, TreeEntry> {
        t.and_then(|t| p.object(t).ok())
            .and_then(|(_, d)| pack::parse_tree(&d).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.name.clone(), e))
            .collect()
    };
    let (a, b) = (read(old), read(new));
    let names: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    for name in names {
        if out.len() >= DIFF_LIMIT {
            return;
        }
        let path = format!("{prefix}{name}");
        match (a.get(name), b.get(name)) {
            (Some(x), Some(y)) if x.oid == y.oid && x.mode == y.mode => {}
            (Some(x), Some(y)) if x.is_dir() && y.is_dir() => {
                diff_trees(p, Some(&x.oid), Some(&y.oid), &format!("{path}/"), out)
            }
            (x, y) => {
                for (e, st) in [(x, 'D'), (y, 'A')] {
                    if let Some(e) = e
                        && e.is_dir()
                    {
                        let (o, n) = if st == 'D' {
                            (Some(e.oid.as_str()), None)
                        } else {
                            (None, Some(e.oid.as_str()))
                        };
                        diff_trees(p, o, n, &format!("{path}/"), out);
                    }
                }
                let file = |e: Option<&TreeEntry>| e.is_some_and(|e| !e.is_dir());
                match (file(x), file(y)) {
                    (true, true) => out.push(('M', path)),
                    (true, false) => out.push(('D', path)),
                    (false, true) => out.push(('A', path)),
                    (false, false) => {}
                }
            }
        }
    }
}

fn e2e_page(owner: &str, repo: &str, rec: &SiteRecord, copies: usize) -> Response {
    let url = git::url_for(owner, repo);
    let body = format!(
        r#"<header><h1>🔒 {repo}</h1><p class="meta">owner {short}… · {copies}/{TARGET_REPLICAS} copies · {size}</p></header>
<p>This repo is <strong>end-to-end encrypted</strong>. The network keeps and serves it, but no gateway or hosting node can read it: files, branches and history are sealed with a key only its owner and the people they share it with hold.</p>
<p>Clone it with the key (the owner prints this URL with <code>c0mpute git share {repo}</code>):</p>
<pre>c0mpute git clone '{url}#key=&lt;key&gt;'</pre>"#,
        repo = esc(repo),
        short = &owner[..12],
        size = human_bytes(rec.size),
        url = esc(&url),
    );
    html(StatusCode::OK, &format!("{repo} (encrypted)"), &body)
}

// ── helpers ───────────────────────────────────────────────────────────────

fn not_found(r: &Repo, why: &str) -> Response {
    let body = format!("{}<p>{}</p>", header_html(r, None), esc(why));
    html(
        StatusCode::NOT_FOUND,
        &format!("Not found · {}", r.repo),
        &body,
    )
}

fn message(status: StatusCode, title: &str, body_html: &str) -> Response {
    html(
        status,
        title,
        &format!("<h1>{}</h1>{body_html}", esc(title)),
    )
}

const CSS: &str = r#"
:root{color-scheme:light dark;--bg:#fff;--fg:#1f2328;--dim:#59636e;--line:#d1d9e0;--link:#0969da;--chip:#f6f8fa}
@media (prefers-color-scheme:dark){:root{--bg:#0d1117;--fg:#e6edf3;--dim:#9198a1;--line:#30363d;--link:#4493f8;--chip:#161b22}}
*{box-sizing:border-box}body{margin:0 auto;max-width:1040px;padding:16px;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif}
a{color:var(--link);text-decoration:none}a:hover{text-decoration:underline}
h1{font-size:1.5em;margin:.2em 0}h1 a{color:var(--fg)}h2{font-size:1.2em}
.meta,.dim{color:var(--dim)}.meta{margin:.2em 0 .6em}
code,pre,.sha{font:13px/1.45 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
pre{margin:0;white-space:pre;overflow-x:auto}pre.wrap{white-space:pre-wrap;word-break:break-word;padding:12px;border:1px solid var(--line);border-radius:6px}
.refs{display:flex;flex-wrap:wrap;gap:6px;margin:.4em 0}.ref{padding:1px 8px;border:1px solid var(--line);border-radius:12px;background:var(--chip);font-size:13px}
.ref.tag::before{content:"🏷 "}.ref.current{border-color:var(--link);font-weight:600}
details.clone{margin:.6em 0;border:1px solid var(--line);border-radius:6px;padding:6px 12px}details.clone pre{padding:8px 0}details.clone p{margin:.2em 0;color:var(--dim);font-size:13px}
summary{cursor:pointer;font-weight:600}
.last{padding:8px 12px;border:1px solid var(--line);border-radius:6px 6px 0 0;margin:.8em 0 0;background:var(--chip);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.crumbs{margin:.6em 0}
table{border-collapse:collapse;width:100%}table.files{border:1px solid var(--line)}table.files td{padding:6px 10px;border-top:1px solid var(--line)}td.ico{width:28px}
table.code{border:1px solid var(--line);border-radius:6px;display:block;overflow-x:auto}table.code td{padding:0 10px;vertical-align:top}td.ln{text-align:right;user-select:none;color:var(--dim);width:1%;white-space:nowrap}td.ln a{color:var(--dim)}tr:target{background:rgba(255,212,0,.18)}
.readme{margin-top:1.2em}.img{max-width:100%}
ol.log{padding-left:1.4em}ol.log li{padding:4px 0;border-bottom:1px solid var(--line)}
ul.changes{list-style:none;padding:0}.st{display:inline-block;width:1.4em;font-weight:700}.stA{color:#1a7f37}.stD{color:#cf222e}.stM{color:#9a6700}
footer{margin-top:2em;color:var(--dim);font-size:13px}
"#;

fn html(status: StatusCode, title: &str, body: &str) -> Response {
    let page = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{}</title><style>{CSS}</style></head><body>{body}<footer>Served by a c0mpute gateway from the signed copies the network keeps. <a href="https://c0mpute.com/docs/git-hosting">About c0mpute git</a></footer></body></html>"#,
        esc(title)
    );
    let mut resp = (status, page).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

pub(crate) fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

/// Percent-encode a path, keeping `/`.
fn enc_path(p: &str) -> String {
    let mut o = String::with_capacity(p.len());
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            o.push(b as char);
        } else {
            let _ = write!(o, "%{b:02X}");
        }
    }
    o
}

fn is_binary(d: &[u8]) -> bool {
    d[..d.len().min(8000)].contains(&0)
}

fn image_type(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        _ => return None,
    })
}

fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn ago(unix: u64) -> String {
    if unix == 0 {
        return "unknown".into();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = now.saturating_sub(unix);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86_399 => format!("{} h ago", s / 3600),
        86_400..=2_591_999 => format!("{} days ago", s / 86_400),
        _ => format!("{} months ago", s / 2_592_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_and_encoding() {
        assert_eq!(
            esc(r#"<a href="x">&'"#),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
        assert_eq!(enc_path("src/a b#?.rs"), "src/a%20b%23%3F.rs");
        assert_eq!(human_bytes(1792), "1.8 KiB");
        assert_eq!(human_bytes(400), "400 B");
        assert!(is_binary(b"a\0b"));
        assert!(!is_binary(b"plain"));
        assert_eq!(image_type("x/Logo.PNG"), Some("image/png"));
        assert_eq!(image_type("a.svg"), None, "svg can script: served as text");
    }

    #[test]
    fn base_url_from_headers() {
        let mut h = HeaderMap::new();
        h.insert(
            header::HOST,
            HeaderValue::from_static("gateway.c0mpute.com"),
        );
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert_eq!(base_url(&h), "https://gateway.c0mpute.com");
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("127.0.0.1:7777"));
        assert_eq!(base_url(&h), "http://127.0.0.1:7777");
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("evil\"<x>"));
        assert_eq!(
            base_url(&h),
            "https://127.0.0.1:7777".replace("https", "http")
        );
    }
}
