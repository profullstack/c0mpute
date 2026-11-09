//! The pushing/fetching side: talks to gateways over HTTP and to the local
//! repository through the `git` binary. Gateways are untrusted: every state
//! is checked against the owner key and every file against its chunk hashes.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use c0mpute_proto::Hash;

use super::*;
use crate::hosting::site::{SiteManifest, SiteRecord};
use c0mpute_sealed::{DataKey, Kind, SealParams, StoragePublic, StorageSecret};
use std::sync::Arc;

/// Gateways to read from and push to, in order of preference.
#[derive(Clone, Debug)]
pub struct Gateways(pub Vec<String>);

impl Gateways {
    pub fn new(list: impl IntoIterator<Item = String>) -> Self {
        let v: Vec<String> = list
            .into_iter()
            .map(|g| g.trim().trim_end_matches('/').to_string())
            .filter(|g| !g.is_empty())
            .collect();
        if v.is_empty() {
            Gateways(vec![
                DEFAULT_GATEWAY.to_string(),
                PUBLIC_GATEWAY.to_string(),
            ])
        } else {
            Gateways(v)
        }
    }

    /// `C0MPUTE_GIT_GATEWAY` (comma-separated), else `git config c0mpute.gateway`
    /// (may repeat), else the local node and then the public gateway.
    pub fn discover(git_dir: Option<&Path>) -> Self {
        if let Ok(v) = std::env::var("C0MPUTE_GIT_GATEWAY")
            && !v.trim().is_empty()
        {
            return Self::new(v.split(',').map(String::from));
        }
        let mut cmd = Command::new("git");
        if let Some(d) = git_dir {
            cmd.arg("--git-dir").arg(d);
        }
        if let Ok(out) = cmd
            .args(["config", "--get-all", "c0mpute.gateway"])
            .output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            return Self::new(text.lines().flat_map(|l| l.split(',')).map(String::from));
        }
        Self::new(Vec::new())
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("http client")
}

/// The newest verified version of `<owner>/<repo>` any gateway knows, with
/// the gateway that served it. `Ok(None)` when every reachable gateway says
/// the repo does not exist.
pub async fn fetch_state(
    gws: &Gateways,
    owner: &str,
    repo: &str,
) -> Result<Option<(Verified, String)>> {
    let client = http();
    let mut best: Option<(Verified, String)> = None;
    let mut errors = Vec::new();
    let mut answered = false;
    for gw in &gws.0 {
        let url = format!("{gw}/git/v1/repos/{owner}/{repo}");
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                errors.push(format!("{gw}: {e}"));
                continue;
            }
        };
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            answered = true;
            continue;
        }
        if !resp.status().is_success() {
            errors.push(format!("{gw}: HTTP {}", resp.status()));
            continue;
        }
        let state: RepoState = match resp.json().await {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!("{gw}: {e}"));
                continue;
            }
        };
        let manifest = match state.verify(owner, repo) {
            Ok(m) => m,
            Err(e) => {
                errors.push(format!("{gw}: {e}"));
                continue;
            }
        };
        answered = true;
        if best
            .as_ref()
            .is_some_and(|(b, _)| b.record.seq >= state.record.seq)
        {
            continue;
        }
        let legacy = is_legacy(&manifest);
        let path = if legacy { LEGACY_REFS } else { DOC_PATH };
        let Some(entry) = manifest.files.get(path) else {
            errors.push(format!("{gw}: site is not a git repo (no {DOC_PATH})"));
            continue;
        };
        let doc = match download(&client, gw, owner, repo, path, entry).await {
            Ok(b) => match if legacy {
                RepoDoc::from_legacy(repo, &b)
            } else {
                RepoDoc::from_bytes(&b)
            } {
                Ok(d) if d.repo == repo => d,
                Ok(_) => {
                    errors.push(format!("{gw}: ref map names another repo"));
                    continue;
                }
                Err(e) => {
                    errors.push(format!("{gw}: {e}"));
                    continue;
                }
            },
            Err(e) => {
                errors.push(format!("{gw}: {e}"));
                continue;
            }
        };
        best = Some((
            Verified {
                record: state.record,
                manifest,
                doc,
                key: None,
                sealed_refs: None,
            },
            gw.clone(),
        ));
    }
    if best.is_none() && !answered {
        bail!("no gateway reachable: {}", errors.join("; "));
    }
    Ok(best)
}

/// Loads this machine's storage identity (the one `c0mpute storage` and `dm`
/// use). Only called for private repos; the CLI caches it, so the password
/// is asked at most once per command.
pub type SecretLoader<'a> = &'a (dyn Fn() -> Result<Arc<StorageSecret>> + Sync);

/// For a private repo: download the sealed ref map, open it with `secret`
/// and fill in refs, head and the repo key. A no-op for public repos.
pub async fn unseal(
    gws: &Gateways,
    state: &mut (Verified, String),
    owner: &str,
    repo: &str,
    secret: &StorageSecret,
) -> Result<()> {
    let (v, gw) = state;
    if !v.doc.sealed || v.key.is_some() {
        return Ok(());
    }
    let entry = v
        .manifest
        .files
        .get(SEALED_REFS)
        .with_context(|| format!("private repo has no {SEALED_REFS}"))?;
    let blob = download_any(gws, gw, owner, repo, SEALED_REFS, entry).await?;
    let (opened, dek) = c0mpute_sealed::open_with_secret(&blob, secret, None).map_err(|_| {
        anyhow!(
            "{} is private and this machine's identity is not one of its readers. Send the owner \
             your `c0mpute storage key show` (c0ps1...) and ask for `c0mpute git share {repo} --with <it>`",
            url_for(owner, repo)
        )
    })?;
    let r: SealedRefs = serde_json::from_slice(&opened.content).context("sealed ref map")?;
    for (name, oid) in &r.refs {
        if !valid_ref_name(name) || !valid_oid(oid) {
            bail!("bad ref {name:?} -> {oid:?} in the sealed ref map");
        }
    }
    if let Some(h) = &r.head
        && !valid_ref_name(h)
    {
        bail!("bad HEAD {h:?} in the sealed ref map");
    }
    v.doc.refs = r.refs;
    v.doc.head = r.head;
    v.key = Some(Arc::new(dek));
    v.sealed_refs = Some(blob);
    Ok(())
}

/// Plaintext of one sealed pack or index.
fn open_sealed(blob: &[u8], key: &DataKey) -> Result<Vec<u8>> {
    let o = c0mpute_sealed::open(blob, key, None)
        .map_err(|e| anyhow!("a sealed pack does not open with the repo key: {e}"))?;
    Ok(o.content.to_vec())
}

/// `(owner, repo)` of the repo with hosting site id `site_id`, from the
/// first gateway whose signed record checks out (the id is a hash of owner
/// and site name, so a gateway cannot substitute another repo).
pub async fn resolve_site(gws: &Gateways, site_id: &str) -> Result<(String, String)> {
    let client = http();
    let mut errors = Vec::new();
    for gw in &gws.0 {
        let url = format!("{gw}/git/v1/sites/{site_id}");
        let state: RepoState = match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => match r.json().await {
                Ok(s) => s,
                Err(e) => {
                    errors.push(format!("{gw}: {e}"));
                    continue;
                }
            },
            Ok(r) => {
                errors.push(format!("{gw}: HTTP {}", r.status()));
                continue;
            }
            Err(e) => {
                errors.push(format!("{gw}: {e}"));
                continue;
            }
        };
        let r = &state.record;
        let Some(repo) = repo_of_site(&r.name) else {
            errors.push(format!("{gw}: site {site_id} is not a git repo"));
            continue;
        };
        if !r.verify() || site::site_id_for(&r.owner, &r.name) != site_id {
            errors.push(format!("{gw}: record does not match site {site_id}"));
            continue;
        }
        return Ok((r.owner.clone(), repo.to_string()));
    }
    bail!(
        "repository {site_id} was not found on the network ({})",
        errors.join("; ")
    )
}

/// One file of a repo, checked against its manifest entry.
async fn download(
    client: &reqwest::Client,
    gw: &str,
    owner: &str,
    repo: &str,
    path: &str,
    entry: &FileEntry,
) -> Result<Vec<u8>> {
    let url = format!("{gw}/git/{owner}/{repo}.git/{path}");
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url}: HTTP {}", resp.status());
    }
    let bytes = resp.bytes().await?.to_vec();
    if !matches_entry(entry, &bytes) {
        bail!("{path} from {gw} does not match its signed hash");
    }
    Ok(bytes)
}

/// Download from the preferred gateway, failing over to the rest.
async fn download_any(
    gws: &Gateways,
    first: &str,
    owner: &str,
    repo: &str,
    path: &str,
    entry: &FileEntry,
) -> Result<Vec<u8>> {
    let client = http();
    let mut last = anyhow!("no gateways");
    for gw in std::iter::once(first).chain(gws.0.iter().map(|s| s.as_str()).filter(|g| *g != first))
    {
        match download(&client, gw, owner, repo, path, entry).await {
            Ok(b) => return Ok(b),
            Err(e) => last = e,
        }
    }
    Err(last)
}

// ── the local repository ──────────────────────────────────────────────────

/// A written pack: base name (`pack-<sha>`), pack bytes, index bytes.
type Pack = (String, Vec<u8>, Vec<u8>);

/// A local repository, driven through the `git` binary.
pub struct Local {
    pub git_dir: PathBuf,
}

impl Local {
    /// `$GIT_DIR` (git sets it for remote helpers), else `git rev-parse --git-dir`.
    pub fn discover() -> Result<Self> {
        if let Ok(d) = std::env::var("GIT_DIR")
            && !d.is_empty()
        {
            return Ok(Self {
                git_dir: PathBuf::from(d),
            });
        }
        let out = Command::new("git")
            .args(["rev-parse", "--absolute-git-dir"])
            .output()
            .context("run git")?;
        if !out.status.success() {
            bail!("not inside a git repository");
        }
        Ok(Self {
            git_dir: PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()),
        })
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new("git");
        c.arg("--git-dir").arg(&self.git_dir);
        c
    }

    /// Run git, returning trimmed stdout; error with stderr on failure.
    pub fn git(&self, args: &[&str]) -> Result<String> {
        let out = self
            .cmd()
            .args(args)
            .stderr(Stdio::piped())
            .output()
            .context("run git")?;
        if !out.status.success() {
            bail!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn git_stdin(&self, args: &[&str], input: &str) -> Result<String> {
        use std::io::Write;
        let mut child = self
            .cmd()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("run git")?;
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn quiet_ok(&self, args: &[&str]) -> bool {
        self.cmd()
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Every object reachable from `wants` is present locally.
    pub fn complete(&self, wants: &[String]) -> bool {
        if wants.is_empty() {
            return true;
        }
        let mut args = vec!["rev-list", "--objects", "--quiet"];
        args.extend(wants.iter().map(|s| s.as_str()));
        args.extend(["--not", "--all"]);
        self.quiet_ok(&args)
    }

    pub fn has_pack(&self, base: &str) -> bool {
        self.git_dir
            .join("objects/pack")
            .join(format!("{base}.idx"))
            .exists()
    }

    pub fn rev_parse(&self, rev: &str) -> Result<String> {
        self.git(&["rev-parse", "--verify", &format!("{rev}^{{object}}")])
    }

    pub fn is_ancestor(&self, old: &str, new: &str) -> bool {
        self.quiet_ok(&["merge-base", "--is-ancestor", old, new])
    }

    fn object_type(&self, oid: &str) -> Option<String> {
        self.git(&["cat-file", "-t", oid]).ok()
    }

    /// Install a downloaded pack + index (pack first: git finds packs by index).
    fn install_pack(&self, base: &str, pack: &[u8], idx: &[u8]) -> Result<()> {
        let dir = self.git_dir.join("objects/pack");
        std::fs::create_dir_all(&dir)?;
        for (ext, bytes) in [("pack", pack), ("idx", idx)] {
            let tmp = dir.join(format!("tmp_c0mpute_{base}.{ext}"));
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, dir.join(format!("{base}.{ext}")))?;
        }
        Ok(())
    }

    /// Unpack a `git bundle` (old repo layout) into the object store.
    pub fn unbundle(&self, bundle: &[u8]) -> Result<()> {
        let dir = self.scratch()?;
        let path = dir.join("repo.bundle");
        let res = std::fs::write(&path, bundle)
            .map_err(anyhow::Error::from)
            .and_then(|_| self.git(&["bundle", "unbundle", &path.to_string_lossy()]));
        let _ = std::fs::remove_dir_all(&dir);
        res.map(|_| ())
    }

    /// Scratch directory inside the repo (never the system temp dir).
    fn scratch(&self) -> Result<PathBuf> {
        let d = self
            .git_dir
            .join(format!("c0mpute-tmp-{}", std::process::id()));
        std::fs::create_dir_all(&d)?;
        Ok(d)
    }

    /// A pack of every object reachable from `wants` and not from `haves`.
    /// `None` when there is nothing to send.
    fn pack(&self, wants: &[String], haves: &[String]) -> Result<Option<Pack>> {
        let dir = self.scratch()?;
        let result = (|| {
            let mut input = String::new();
            for w in wants {
                input.push_str(&format!("{w}\n"));
            }
            for h in haves {
                input.push_str(&format!("^{h}\n"));
            }
            let base = dir.join("pack");
            let sha = self.git_stdin(
                &[
                    "pack-objects",
                    "--revs",
                    "--delta-base-offset",
                    "-q",
                    &base.to_string_lossy(),
                ],
                &input,
            )?;
            let sha = sha.lines().last().unwrap_or("").trim().to_string();
            if !valid_oid(&sha) {
                bail!("git pack-objects printed no pack name");
            }
            let name = format!("pack-{sha}");
            let pack = std::fs::read(dir.join(format!("{name}.pack")))?;
            let idx = std::fs::read(dir.join(format!("{name}.idx")))?;
            // Header: "PACK", version, object count (big endian).
            let objects = pack
                .get(8..12)
                .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
                .unwrap_or(0);
            Ok(if objects == 0 {
                None
            } else {
                Some((name, pack, idx))
            })
        })();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }
}

/// Make every object reachable from `wants` local, downloading the repo's
/// packs newest first until nothing is missing.
pub async fn fetch_objects(
    local: &Local,
    gws: &Gateways,
    state: &(Verified, String),
    owner: &str,
    repo: &str,
    wants: &[String],
) -> Result<usize> {
    let (v, gw) = state;
    let mut downloaded = 0;
    if local.complete(wants) {
        return Ok(0);
    }
    if v.doc.sealed && v.key.is_none() {
        bail!("private repo: unseal it before fetching");
    }
    for base in v.doc.packs.iter().rev() {
        if local.has_pack(base) {
            continue;
        }
        let (pp, ip) = (pack_path(base), idx_path(base));
        let (Some(pe), Some(ie)) = (v.manifest.files.get(&pp), v.manifest.files.get(&ip)) else {
            bail!("ref map names {base} but the repo has no such pack");
        };
        let mut pack = download_any(gws, gw, owner, repo, &pp, pe).await?;
        let mut idx = download_any(gws, gw, owner, repo, &ip, ie).await?;
        if let Some(k) = &v.key {
            pack = open_sealed(&pack, k)?;
            idx = open_sealed(&idx, k)?;
        }
        local.install_pack(base, &pack, &idx)?;
        downloaded += 1;
        if local.complete(wants) {
            return Ok(downloaded);
        }
    }
    // The old bundle layout: one bundle holds every object.
    if let Some(be) = v.manifest.files.get(LEGACY_BUNDLE)
        && !local.complete(wants)
    {
        let bundle = download_any(gws, gw, owner, repo, LEGACY_BUNDLE, be).await?;
        local.unbundle(&bundle)?;
        downloaded += 1;
    }
    if !local.complete(wants) {
        bail!("the repo's packs do not contain every object its refs need");
    }
    Ok(downloaded)
}

/// One `[+]<src>:<dst>` of a push.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushSpec {
    pub force: bool,
    /// Empty = delete `dst`.
    pub src: String,
    pub dst: String,
}

impl PushSpec {
    pub fn parse(s: &str) -> Result<Self> {
        let (force, rest) = match s.strip_prefix('+') {
            Some(r) => (true, r),
            None => (false, s),
        };
        let (src, dst) = rest
            .split_once(':')
            .with_context(|| format!("bad refspec {s:?}"))?;
        if !valid_ref_name(dst) {
            bail!("bad destination ref {dst:?}");
        }
        Ok(Self {
            force,
            src: src.to_string(),
            dst: dst.to_string(),
        })
    }
}

/// What a push did.
#[derive(Debug)]
pub struct PushReport {
    /// Per destination ref: `Ok` or the reason it was refused.
    pub results: Vec<(String, std::result::Result<(), String>)>,
    pub site_id: String,
    /// The gateway that accepted it (`None` when nothing needed sending).
    pub gateway: Option<String>,
    pub seq: u64,
    pub holders: usize,
    /// The commit HEAD's branch now points at, when this push moved it.
    pub head_moved: Option<String>,
}

/// Private/public mode and readers for [`push`].
#[derive(Default)]
pub struct PushOptions<'a> {
    /// This machine's storage identity, for private repos.
    pub secret: Option<SecretLoader<'a>>,
    /// Mode for a NEW repo: `Some(true)` public, otherwise private. An
    /// existing repo keeps its mode; asking for the other one is an error.
    pub public: Option<bool>,
    /// Private repos: identities to add as readers (key header only).
    pub add_readers: Vec<StoragePublic>,
    /// Private repos: identities to remove. Re-keys the repo and re-seals
    /// every pack, since a removed reader may already hold the old key.
    pub remove_readers: Vec<StoragePublic>,
}

/// Push `specs` from `local` to `<owner>/<repo>`, signed with `keypair`
/// (which must be the owner's).
pub async fn push(
    local: &Local,
    gws: &Gateways,
    keypair: &c0mpute_net::libp2p::identity::Keypair,
    owner: &str,
    repo: &str,
    specs: &[PushSpec],
    opts: &PushOptions<'_>,
) -> Result<PushReport> {
    let me = crate::hosting::site::owner_hex(keypair)
        .context("an ed25519 identity is needed to push")?;
    let site_id = site_id(owner, repo);
    if me != owner {
        bail!(
            "{} belongs to key {owner}; this machine's key is {me}",
            url_for(owner, repo)
        );
    }
    let t0 = std::time::Instant::now();
    let mut state = fetch_state(gws, owner, repo).await?;
    trace(&format!("state fetched in {:?}", t0.elapsed()));
    if specs.is_empty() && state.is_none() {
        bail!("{} does not exist (push something first)", url_for(owner, repo));
    }
    let private = match &state {
        Some((v, _)) => {
            if opts.public == Some(v.doc.sealed) {
                if v.doc.sealed {
                    bail!(
                        "{} is private; making it public would publish its whole history. \
                         Push to a new repo with `git config c0mpute.public true` instead",
                        url_for(owner, repo)
                    );
                }
                bail!(
                    "{} is public; its history is already readable. Push to a new repo \
                     (private by default) instead",
                    url_for(owner, repo)
                );
            }
            v.doc.sealed
        }
        None => opts.public != Some(true),
    };
    if !private && !(opts.add_readers.is_empty() && opts.remove_readers.is_empty()) {
        bail!("{} is public: there are no readers to change", url_for(owner, repo));
    }
    let secret: Option<Arc<StorageSecret>> = if private {
        let load = opts.secret.context(
            "private repos need this machine's c0mpute identity (run `c0mpute storage key init`)",
        )?;
        Some(load()?)
    } else {
        None
    };
    if let (Some(st), Some(sec)) = (state.as_mut(), secret.as_ref()) {
        unseal(gws, st, owner, repo, sec).await?;
    }
    let rekey = !opts.remove_readers.is_empty();
    let readers_change = !opts.add_readers.is_empty() || rekey;
    let remote_refs: BTreeMap<String, String> = state
        .as_ref()
        .map(|(v, _)| v.doc.refs.clone())
        .unwrap_or_default();
    let tips: Vec<String> = {
        let mut t: Vec<String> = remote_refs.values().cloned().collect();
        t.sort();
        t.dedup();
        t
    };
    if let Some(s) = &state
        && (!specs.is_empty() || rekey)
        && !local.complete(&tips)
    {
        fetch_objects(local, gws, s, owner, repo, &tips).await?;
    }

    let mut new_refs = remote_refs.clone();
    let mut results = Vec::new();
    let mut forced = false;
    for spec in specs {
        if spec.src.is_empty() {
            new_refs.remove(&spec.dst);
            results.push((spec.dst.clone(), Ok(())));
            continue;
        }
        let new = match local.rev_parse(&spec.src) {
            Ok(n) => n,
            Err(e) => {
                results.push((spec.dst.clone(), Err(format!("{e}"))));
                continue;
            }
        };
        if let Some(old) = remote_refs.get(&spec.dst)
            && *old != new
        {
            if spec.force {
                forced = true;
            } else if spec.dst.starts_with("refs/tags/") {
                results.push((spec.dst.clone(), Err("already exists".into())));
                continue;
            } else if !local.is_ancestor(old, &new) {
                results.push((spec.dst.clone(), Err("non-fast-forward".into())));
                continue;
            }
        }
        new_refs.insert(spec.dst.clone(), new);
        results.push((spec.dst.clone(), Ok(())));
    }
    let report = |gateway, seq, holders, results| PushReport {
        results,
        site_id: site_id.clone(),
        gateway,
        seq,
        holders,
        head_moved: None,
    };
    if new_refs == remote_refs && state.is_some() && !readers_change {
        let seq = state.as_ref().map(|(v, _)| v.record.seq).unwrap_or(0);
        return Ok(report(None, seq, 0, results));
    }
    if !results.is_empty() && results.iter().all(|(_, r)| r.is_err()) {
        return Ok(report(None, 0, 0, results));
    }

    // Files carried over from the current version, and the new ones.
    let (old_packs, mut files, prev, old_seq, domains) = match &state {
        Some((v, _)) => (
            v.doc.packs.clone(),
            v.manifest.files.clone(),
            Some(v.record.manifest.clone()),
            v.record.seq,
            v.record.domains.clone(),
        ),
        None => (Vec::new(), BTreeMap::new(), None, 0, Vec::new()),
    };
    files.retain(|p, _| p.starts_with("objects/pack/"));
    let wants: Vec<String> = {
        let mut w: Vec<String> = new_refs.values().cloned().collect();
        w.sort();
        w.dedup();
        w
    };
    let head = state
        .as_ref()
        .and_then(|(v, _)| v.doc.head.clone())
        .filter(|h| new_refs.contains_key(h))
        .or_else(|| {
            ["refs/heads/main", "refs/heads/master"]
                .iter()
                .find(|h| new_refs.contains_key(**h))
                .map(|h| h.to_string())
        })
        .or_else(|| {
            new_refs
                .keys()
                .find(|k| k.starts_with("refs/heads/"))
                .cloned()
        });

    // A private repo: seal the ref map first, which fixes the repo key (new
    // on the first push and on a re-key), then seal every new pack with it.
    let mut sealed_refs_blob: Option<Vec<u8>> = None;
    let mut key: Option<DataKey> = None;
    if let Some(sec) = &secret {
        let mut readers: Vec<StoragePublic> = vec![sec.public()];
        if let Some(old) = state.as_ref().and_then(|(v, _)| v.sealed_refs.as_deref()) {
            for r in c0mpute_sealed::roster(old, sec).map_err(|e| anyhow!("reader list: {e}"))? {
                if !readers.contains(&r) {
                    readers.push(r);
                }
            }
        }
        for r in &opts.add_readers {
            if !readers.contains(r) {
                readers.push(*r);
            }
        }
        readers.retain(|r| *r == sec.public() || !opts.remove_readers.contains(r));
        let refs_json = serde_json::to_vec(&SealedRefs {
            head: head.clone(),
            refs: new_refs.clone(),
        })?;
        let name = sealed_name(sec, repo);
        let params = SealParams {
            kind: Kind::File,
            name: &name,
            owner: sec,
            file_name: None,
            content_type: None,
            files: BTreeMap::new(),
            parts: Vec::new(),
            recipients: &readers,
            created: now_ms() / 1000,
            version: 0,
        };
        let existing = state.as_ref().and_then(|(v, _)| v.key.clone());
        let sealed = match existing {
            Some(k) if !rekey => c0mpute_sealed::seal_with_key(&refs_json, &params, k.duplicate()),
            _ => c0mpute_sealed::seal(&refs_json, &params),
        }
        .map_err(|e| anyhow!("seal the ref map: {e}"))?;
        sealed_refs_blob = Some(sealed.blob);
        key = Some(sealed.dek);
    }
    let seal_file = |bytes: &[u8]| -> Result<Vec<u8>> {
        let (Some(sec), Some(k)) = (&secret, &key) else {
            return Ok(bytes.to_vec());
        };
        let name = sealed_name(sec, repo);
        let params = SealParams {
            kind: Kind::File,
            name: &name,
            owner: sec,
            file_name: None,
            content_type: None,
            files: BTreeMap::new(),
            parts: Vec::new(),
            recipients: &[],
            created: now_ms() / 1000,
            version: 0,
        };
        Ok(c0mpute_sealed::seal_with_key(bytes, &params, k.duplicate())
            .map_err(|e| anyhow!("seal a pack: {e}"))?
            .blob)
    };
    let mut packs = old_packs.clone();
    let mut new_files: Vec<(String, Vec<u8>)> = Vec::new();
    // An old bundle-layout repo becomes one full pack (its bundle is dropped).
    let legacy = state.as_ref().is_some_and(|(v, _)| is_legacy(&v.manifest));
    // A re-key re-seals everything: one full pack under the new key.
    let repack = legacy || rekey || old_packs.len() + 1 > REPACK_AFTER;
    let made = if repack {
        files.clear();
        packs.clear();
        local.pack(&wants, &[])?
    } else if new_refs == remote_refs {
        None // readers changed, objects did not
    } else {
        local.pack(&wants, &tips)?
    };
    if let Some((real, pack, idx)) = made {
        let name = if private { random_pack_name() } else { real };
        if !packs.contains(&name) {
            new_files.push((pack_path(&name), seal_file(&pack)?));
            new_files.push((idx_path(&name), seal_file(&idx)?));
            packs.push(name);
        }
    }

    let mut peeled = BTreeMap::new();
    for (name, oid) in &new_refs {
        if name.starts_with("refs/tags/") && local.object_type(oid).as_deref() == Some("tag") {
            peeled.insert(oid.clone(), local.rev_parse(&format!("{oid}^{{}}"))?);
        }
    }
    let head_moved = head
        .as_ref()
        .and_then(|h| new_refs.get(h).filter(|n| remote_refs.get(h) != Some(*n)))
        .cloned();
    let doc = if let Some(blob) = sealed_refs_blob {
        // Gateways see pack names, `prev` and `force`; refs only sealed.
        new_files.push((SEALED_REFS.into(), blob));
        RepoDoc {
            v: 2,
            repo: repo.to_string(),
            head: None,
            refs: BTreeMap::new(),
            packs,
            prev,
            force: forced || repack,
            pushed_at: now_ms(),
            sealed: true,
        }
    } else {
        let doc = RepoDoc {
            v: 1,
            repo: repo.to_string(),
            head,
            refs: new_refs,
            packs,
            prev,
            force: forced || repack,
            pushed_at: now_ms(),
            sealed: false,
        };
        new_files.push(("HEAD".into(), doc.head_file().into_bytes()));
        new_files.push(("info/refs".into(), doc.info_refs(&peeled).into_bytes()));
        new_files.push(("objects/info/packs".into(), doc.packs_file().into_bytes()));
        doc
    };
    new_files.push((DOC_PATH.into(), doc.to_bytes()));

    let mut chunks = Vec::new();
    let mut seen = HashSet::new();
    for (path, bytes) in &new_files {
        let (entry, cs) = file_entry(path, bytes);
        for (h, c) in entry.chunks.iter().zip(cs) {
            if seen.insert(h.clone()) {
                chunks.push(c);
            }
        }
        files.insert(path.clone(), entry);
    }
    let manifest = SiteManifest {
        v: 1,
        name: site_name(repo),
        files,
    };
    let total = manifest.total_bytes();
    if total > MAX_REPO_BYTES {
        bail!(
            "repo would be {} MiB, over the {} MiB limit for a repo (keep large binaries out of git)",
            total >> 20,
            MAX_REPO_BYTES >> 20
        );
    }
    let mbytes = manifest.to_bytes();
    let seq = now_ms().max(old_seq + 1);
    let record = SiteRecord::sign(
        keypair,
        &site_name(repo),
        &Hash::of(&mbytes),
        seq,
        total,
        domains,
    )?;
    let header = PushHeader {
        record,
        manifest: String::from_utf8(mbytes).expect("manifest is JSON"),
    };
    let body = encode_push(&header, &chunks);
    trace(&format!(
        "packed and signed at {:?} ({} bytes to send)",
        t0.elapsed(),
        body.len()
    ));

    let client = http();
    let mut errors = Vec::new();
    for gw in &gws.0 {
        let url = format!("{gw}/git/v1/repos/{owner}/{repo}");
        match client
            .post(&url)
            .header("content-type", "application/x-c0mpute-git-push")
            .body(body.clone())
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let v: serde_json::Value = r.json().await.unwrap_or_default();
                let holders = v["holders"].as_u64().unwrap_or(0) as usize;
                trace(&format!("accepted by {gw} at {:?}", t0.elapsed()));
                let mut r = report(Some(gw.clone()), seq, holders, results);
                r.head_moved = head_moved;
                return Ok(r);
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                errors.push(format!("{gw}: HTTP {status}: {}", text.trim()));
            }
            Err(e) => errors.push(format!("{gw}: {e}")),
        }
    }
    let why = errors.join("; ");
    if results.is_empty() {
        bail!("no gateway accepted the change: {why}");
    }
    let results = results
        .into_iter()
        .map(|(d, r)| (d, r.and(Err(why.clone()))))
        .collect();
    Ok(report(None, seq, 0, results))
}

/// Phase timings on stderr when `C0MPUTE_GIT_TRACE` is set.
fn trace(msg: &str) {
    if std::env::var_os("C0MPUTE_GIT_TRACE").is_some() {
        eprintln!("c0mpute: {msg}");
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Repos a gateway knows: `(owner, repo, site_id, seq, copies)`.
pub async fn list(gw: &str) -> Result<Vec<serde_json::Value>> {
    let url = format!("{}/git/v1/repos", gw.trim_end_matches('/'));
    let v: serde_json::Value = http()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .json()
        .await?;
    Ok(v["repos"].as_array().cloned().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_specs() {
        let s = PushSpec::parse("+refs/heads/a:refs/heads/b").unwrap();
        assert!(s.force);
        assert_eq!(s.src, "refs/heads/a");
        let d = PushSpec::parse(":refs/heads/gone").unwrap();
        assert!(d.src.is_empty() && !d.force);
        assert!(PushSpec::parse("refs/heads/a").is_err());
        assert!(PushSpec::parse("a:HEAD").is_err());
    }

    #[test]
    fn gateways_default_and_trim() {
        assert_eq!(
            Gateways::new(Vec::new()).0,
            vec![DEFAULT_GATEWAY.to_string(), PUBLIC_GATEWAY.to_string()]
        );
        assert_eq!(
            Gateways::new(vec![" http://a:1/ ".into(), "".into()]).0,
            vec!["http://a:1".to_string()]
        );
    }
}
