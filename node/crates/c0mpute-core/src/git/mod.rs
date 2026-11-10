//! Git hosting on the hosting network: a repository is a hosting site.
//!
//! * A repo `<repo>` owned by ed25519 key `<owner>` is the hosting site named
//!   `git-<repo>` of that owner, so its site id, owner signature, versioning
//!   (`seq`), three self-healing copies, proxying from any gateway and the
//!   d1sks marketplace all come from [`crate::hosting`] unchanged. Hosting
//!   nodes that know nothing about git replicate repos already.
//! * The site's files are a "dumb HTTP" git repository: `HEAD`, `info/refs`,
//!   `objects/info/packs` and the packfiles (`objects/pack/pack-<sha>.pack`
//!   and `.idx`), plus [`DOC_PATH`], the ref map ([`RepoDoc`]). The owner's
//!   record signs the manifest, the manifest pins every file by hash, so the
//!   ref map and every pack are signed transitively.
//! * Each push appends one pack holding only the new objects, and the ref
//!   map names the manifest it replaces (`prev`), so a gateway can refuse a
//!   push built on a stale view (no lost updates) and a non-forced push that
//!   drops packs. After [`REPACK_AFTER`] packs the client writes one full pack.
//! * Reads need no git on the gateway: `git clone https://<gateway>/git/<owner>/<repo>.git`
//!   uses git's dumb HTTP protocol, and `git clone c0mpute://<owner>/<repo>`
//!   goes through `git-remote-c0mpute` ([`helper`]), which also verifies the
//!   owner signature and every chunk hash itself, so gateways are untrusted.

pub mod client;
pub mod deploy;
pub mod helper;
pub mod keys;
pub mod pack;

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};

use crate::hosting::site::{self, FILE_CHUNK_BYTES, FileEntry, SiteManifest, SiteRecord};

/// Site names of git repos start with this, so a repo never collides with a
/// static site of the same owner.
pub const SITE_PREFIX: &str = "git-";
/// The ref map file inside a repo's site.
pub const DOC_PATH: &str = "c0mpute-git.json";
/// The URL scheme of the remote helper.
pub const SCHEME: &str = "c0mpute";
/// Packs a repo may grow to before the next push writes one full pack.
pub const REPACK_AFTER: usize = 16;
/// Magic at the start of a push body.
pub const PUSH_MAGIC: &[u8] = b"C0GIT1\n";
/// Default gateway when nothing is configured: the local node.
pub const DEFAULT_GATEWAY: &str = "http://127.0.0.1:7777";
/// Largest repo: the size above which hosting stripes a site across nodes.
/// A push is held whole by the gateway it lands on, so repos stay unstriped.
pub const MAX_REPO_BYTES: u64 = site::STRIPE_ABOVE;
/// Tried after the local node when no gateway is configured, so a machine
/// that runs no node can still push and clone.
pub const PUBLIC_GATEWAY: &str = "https://gateway.c0mpute.com";

/// The first git helper (v0.2.99 to v0.3.x) stored a repo as these two files:
/// a ref map and one `git bundle` of everything. Such repos still clone, and
/// the first push to one rewrites it in the pack layout (one full pack).
pub const LEGACY_REFS: &str = "refs.json";
pub const LEGACY_BUNDLE: &str = "repo.bundle";

#[derive(Deserialize)]
struct LegacyRefs {
    #[serde(default)]
    head: Option<String>,
    refs: BTreeMap<String, String>,
}

/// True when a repo version is in the old bundle layout.
pub fn is_legacy(m: &SiteManifest) -> bool {
    !m.files.contains_key(DOC_PATH)
        && m.files.contains_key(LEGACY_REFS)
        && m.files.contains_key(LEGACY_BUNDLE)
}

/// Longest repo name (the site name limit minus [`SITE_PREFIX`]).
pub fn max_repo_chars() -> usize {
    site::MAX_NAME_CHARS - SITE_PREFIX.len()
}

/// A repo name: what a site name allows (a-z, 0-9, '-'), 1-59 characters.
pub fn valid_repo(repo: &str) -> bool {
    repo.chars().count() <= max_repo_chars() && site::valid_name(repo)
}

/// The hosting site name of a repo.
pub fn site_name(repo: &str) -> String {
    format!("{SITE_PREFIX}{repo}")
}

/// The repo a hosting site name stands for, when it is a git repo's.
pub fn repo_of_site(name: &str) -> Option<&str> {
    name.strip_prefix(SITE_PREFIX).filter(|r| valid_repo(r))
}

/// The site id of `<owner>/<repo>`.
pub fn site_id(owner_hex: &str, repo: &str) -> String {
    site::site_id_for(owner_hex, &site_name(repo))
}

pub fn valid_owner(owner: &str) -> bool {
    owner.len() == 64
        && owner
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// `<repo>` or `<repo>.git` from a URL path segment.
pub fn strip_dot_git(s: &str) -> &str {
    s.strip_suffix(".git").unwrap_or(s)
}

/// `c0mpute://<owner>/<repo>[.git]`, or `c0mpute://<repo>` for a repo of
/// this machine's own key (`owner = None`), or `c0mpute://<20-hex site id>`
/// (the URL form of the first, bundle-based helper: `site` is set, `owner`
/// and `repo` are learned from the signed record, see [`client::resolve_site`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoUrl {
    pub owner: Option<String>,
    pub repo: String,
    pub site: Option<String>,
}

/// A hosting site id: 20 lowercase hex characters.
pub fn valid_site_id(s: &str) -> bool {
    s.len() == 20
        && s.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

impl RepoUrl {
    pub fn parse(url: &str) -> Result<Self> {
        let rest = url
            .strip_prefix(&format!("{SCHEME}://"))
            .or_else(|| url.strip_prefix(&format!("{SCHEME}::")))
            .or_else(|| url.strip_prefix(&format!("{SCHEME}:")))
            .unwrap_or(url)
            .trim_end_matches('/');
        let parts: Vec<&str> = rest.split('/').collect();
        if let [one] = parts.as_slice()
            && valid_site_id(strip_dot_git(one))
        {
            return Ok(Self {
                owner: None,
                repo: String::new(),
                site: Some(strip_dot_git(one).to_string()),
            });
        }
        let (owner, repo) = match parts.as_slice() {
            // `c0mpute://git-<name>` was accepted for `<name>` before.
            [repo] => {
                let r = strip_dot_git(repo);
                (
                    None,
                    r.strip_prefix(SITE_PREFIX)
                        .filter(|r| valid_repo(r))
                        .unwrap_or(r),
                )
            }
            [owner, repo] => (Some(owner.to_ascii_lowercase()), strip_dot_git(repo)),
            _ => bail!("expected {SCHEME}://<owner-key>/<repo>, got {url:?}"),
        };
        if let Some(o) = &owner
            && !valid_owner(o)
        {
            bail!("owner must be a 64-hex ed25519 key, got {o:?}");
        }
        if !valid_repo(repo) {
            bail!(
                "repo name must be 1-{} of a-z, 0-9 and '-', got {repo:?}",
                max_repo_chars()
            );
        }
        Ok(Self {
            owner,
            repo: repo.to_string(),
            site: None,
        })
    }

    /// The owner key, defaulting to `me`.
    pub fn owner_or<'a>(&'a self, me: &'a str) -> &'a str {
        self.owner.as_deref().unwrap_or(me)
    }
}

pub fn url_for(owner: &str, repo: &str) -> String {
    format!("{SCHEME}://{owner}/{repo}")
}

/// The signed ref map of one repo version ([`DOC_PATH`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDoc {
    pub v: u8,
    pub repo: String,
    /// Symbolic HEAD, e.g. `refs/heads/main`.
    pub head: Option<String>,
    /// Ref name -> object id (hex).
    pub refs: BTreeMap<String, String>,
    /// Pack base names (`pack-<sha>`), oldest first.
    pub packs: Vec<String>,
    /// Manifest hash of the version this push replaces (`None` for the first).
    pub prev: Option<String>,
    /// The pusher allowed refs to move backwards / packs to go.
    #[serde(default)]
    pub force: bool,
    /// Milliseconds since the epoch.
    #[serde(default)]
    pub pushed_at: u64,
    /// A private repo (doc version 2): `refs` and `head` are empty here and
    /// live in [`SEALED_REFS`], and every pack and index is sealed under the
    /// repo key. Gateways only see pack names (random), sizes and `prev`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sealed: bool,
}

/// The sealed ref map of a private repo: `c0mpute-sealed` ciphertext of a
/// [`SealedRefs`], with key slots for the owner and everyone it is shared with.
pub const SEALED_REFS: &str = "c0mpute-git.sealed";

/// What [`SEALED_REFS`] holds once opened.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedRefs {
    pub head: Option<String>,
    pub refs: BTreeMap<String, String>,
}

/// A random pack name for a private repo (a real one would leak the pack's
/// checksum, which identifies its content to anyone holding the same objects).
pub fn random_pack_name() -> String {
    let b: [u8; 20] = rand::random();
    format!("pack-{}", hex::encode(b))
}

/// The stable, unlinkable name a private repo's sealed files are signed under.
pub fn sealed_name(owner: &c0mpute_sealed::StorageSecret, repo: &str) -> String {
    owner.named_object(&format!("git:{repo}"))
}

impl RepoDoc {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self).expect("repo doc serializes")
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        let d: RepoDoc = serde_json::from_slice(b).context("c0mpute-git.json is not a repo doc")?;
        match (d.v, d.sealed) {
            (1, false) => {}
            (2, true) => {
                if !d.refs.is_empty() || d.head.is_some() {
                    bail!("a private repo doc carries plaintext refs");
                }
            }
            (v, _) => bail!("unsupported repo doc version {v} (update c0mpute)"),
        }
        for (name, oid) in &d.refs {
            if !valid_ref_name(name) || !valid_oid(oid) {
                bail!("bad ref {name:?} -> {oid:?}");
            }
        }
        if let Some(h) = &d.head
            && !valid_ref_name(h)
        {
            bail!("bad HEAD {h:?}");
        }
        if !d.packs.iter().all(|p| valid_pack_name(p)) {
            bail!("bad pack name");
        }
        Ok(d)
    }

    /// The ref map of an old bundle-layout repo (its `refs.json`), as a doc
    /// with no packs: every object is in [`LEGACY_BUNDLE`].
    pub fn from_legacy(repo: &str, refs_json: &[u8]) -> Result<Self> {
        let l: LegacyRefs = serde_json::from_slice(refs_json).context("refs.json")?;
        let d = RepoDoc {
            v: 1,
            repo: repo.to_string(),
            head: l.head,
            refs: l.refs,
            packs: Vec::new(),
            prev: None,
            force: false,
            pushed_at: 0,
            sealed: false,
        };
        // Same validation as a real doc.
        Self::from_bytes(&d.to_bytes())
    }

    /// `info/refs` for git's dumb HTTP protocol. `peeled` maps an annotated
    /// tag's id to the commit it points at.
    pub fn info_refs(&self, peeled: &BTreeMap<String, String>) -> String {
        let mut out = String::new();
        for (name, oid) in &self.refs {
            out.push_str(&format!("{oid}\t{name}\n"));
            if let Some(p) = peeled.get(oid)
                && name.starts_with("refs/tags/")
            {
                out.push_str(&format!("{p}\t{name}^{{}}\n"));
            }
        }
        out
    }

    pub fn head_file(&self) -> String {
        match &self.head {
            Some(h) => format!("ref: {h}\n"),
            None => "ref: refs/heads/main\n".to_string(),
        }
    }

    pub fn packs_file(&self) -> String {
        let mut out: String = self.packs.iter().map(|p| format!("P {p}.pack\n")).collect();
        out.push('\n');
        out
    }
}

/// A hex SHA-1 (or SHA-256) object id.
pub fn valid_oid(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64)
        && s.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// A ref name we store: `refs/...`, git's own rules, kept strict.
pub fn valid_ref_name(s: &str) -> bool {
    s.starts_with("refs/")
        && s.len() <= 255
        && !s.ends_with('/')
        && !s.ends_with(".lock")
        && !s.contains("..")
        && !s.contains("//")
        && !s.contains("@{")
        && s.split('/').all(|c| !c.is_empty() && !c.starts_with('.'))
        && !s.chars().any(|c| c.is_control() || " ~^:?*[\\".contains(c))
}

pub fn valid_pack_name(s: &str) -> bool {
    s.strip_prefix("pack-").is_some_and(valid_oid)
}

pub fn pack_path(base: &str) -> String {
    format!("objects/pack/{base}.pack")
}

pub fn idx_path(base: &str) -> String {
    format!("objects/pack/{base}.idx")
}

/// A file entry plus its chunk bytes, cut exactly as hosting cuts files.
pub fn file_entry(path: &str, bytes: &[u8]) -> (FileEntry, Vec<Vec<u8>>) {
    let mut hashes = Vec::new();
    let mut chunks = Vec::new();
    if bytes.is_empty() {
        hashes.push(Hash::of(&[]).to_hex());
        chunks.push(Vec::new());
    }
    for piece in bytes.chunks(FILE_CHUNK_BYTES) {
        hashes.push(Hash::of(piece).to_hex());
        chunks.push(piece.to_vec());
    }
    let content_type = if path.ends_with(".pack") || path.ends_with(".idx") {
        "application/octet-stream"
    } else if path.ends_with(".json") {
        "application/json"
    } else {
        "text/plain; charset=utf-8"
    };
    (
        FileEntry {
            size: bytes.len() as u64,
            content_type: content_type.to_string(),
            chunks: hashes,
        },
        chunks,
    )
}

/// True when `bytes` are exactly the file `entry` describes.
pub fn matches_entry(entry: &FileEntry, bytes: &[u8]) -> bool {
    if bytes.len() as u64 != entry.size {
        return false;
    }
    let mut hashes: Vec<String> = bytes
        .chunks(FILE_CHUNK_BYTES)
        .map(|p| Hash::of(p).to_hex())
        .collect();
    if bytes.is_empty() {
        hashes.push(Hash::of(&[]).to_hex());
    }
    hashes == entry.chunks
}

/// What a gateway reports for one repo: the signed record and the manifest
/// bytes it points at (the client checks both).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoState {
    pub record: SiteRecord,
    /// The manifest JSON exactly as stored (its blake3 is `record.manifest`).
    pub manifest: String,
    #[serde(default)]
    pub holders: Vec<String>,
}

/// A repo version checked end to end against its owner's key.
#[derive(Clone, Debug)]
pub struct Verified {
    pub record: SiteRecord,
    pub manifest: SiteManifest,
    /// For a private repo, after [`client::unseal`]: refs and head filled in
    /// from [`SEALED_REFS`] (in memory only; the published doc has none).
    pub doc: RepoDoc,
    /// A private repo's key, once unsealed.
    pub key: Option<std::sync::Arc<c0mpute_sealed::DataKey>>,
    /// A private repo's sealed ref map as published (its key slots say who
    /// can read it).
    pub sealed_refs: Option<Vec<u8>>,
}

impl RepoState {
    /// Check signature, owner, name and manifest hash. The ref map itself is
    /// read separately (it is a file of the site).
    pub fn verify(&self, owner: &str, repo: &str) -> Result<SiteManifest> {
        let r = &self.record;
        if !r.verify() {
            bail!("repo record signature does not verify");
        }
        if r.owner != owner || r.name != site_name(repo) {
            bail!("gateway returned a record for another repo");
        }
        if Hash::of(self.manifest.as_bytes()).to_hex() != r.manifest {
            bail!("manifest does not match the signed record");
        }
        let m = SiteManifest::from_bytes(self.manifest.as_bytes())?;
        if m.total_bytes() != r.size || m.name != r.name {
            bail!("manifest does not match its record");
        }
        Ok(m)
    }
}

/// The body of `POST /git/v1/repos/<owner>/<repo>`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PushHeader {
    pub record: SiteRecord,
    pub manifest: String,
}

/// `PUSH_MAGIC`, u32-BE header length, header JSON, then chunks as u32-BE
/// length + bytes.
pub fn encode_push(header: &PushHeader, chunks: &[Vec<u8>]) -> Vec<u8> {
    let h = serde_json::to_vec(header).expect("push header serializes");
    let mut out = Vec::with_capacity(
        PUSH_MAGIC.len() + 4 + h.len() + chunks.iter().map(|c| c.len() + 4).sum::<usize>(),
    );
    out.extend_from_slice(PUSH_MAGIC);
    out.extend_from_slice(&(h.len() as u32).to_be_bytes());
    out.extend_from_slice(&h);
    for c in chunks {
        out.extend_from_slice(&(c.len() as u32).to_be_bytes());
        out.extend_from_slice(c);
    }
    out
}

pub fn decode_push(body: &[u8]) -> Result<(PushHeader, Vec<Vec<u8>>)> {
    fn take<'a>(rest: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
        if rest.len() < n {
            bail!("truncated push body");
        }
        let (head, tail) = rest.split_at(n);
        *rest = tail;
        Ok(head)
    }
    fn len(rest: &mut &[u8]) -> Result<usize> {
        let b = take(rest, 4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }
    let mut rest = body
        .strip_prefix(PUSH_MAGIC)
        .context("not a c0mpute git push")?;
    let hlen = len(&mut rest)?;
    if hlen > 4 * 1024 * 1024 {
        bail!("push header too large");
    }
    let header: PushHeader =
        serde_json::from_slice(take(&mut rest, hlen)?).context("push header")?;
    let mut chunks = Vec::new();
    while !rest.is_empty() {
        let n = len(&mut rest)?;
        if n > FILE_CHUNK_BYTES {
            bail!("chunk over {FILE_CHUNK_BYTES} bytes");
        }
        chunks.push(take(&mut rest, n)?.to_vec());
    }
    Ok((header, chunks))
}

/// Server-side ref-update rules for a push of `new` over `old` (both
/// verified, same repo). Signatures are checked by the caller.
pub fn check_update(
    old: Option<(&SiteRecord, &SiteManifest, &RepoDoc)>,
    new: &SiteRecord,
    new_m: &SiteManifest,
    doc: &RepoDoc,
) -> Result<()> {
    // Every file the doc names must be in the manifest.
    for p in &doc.packs {
        if !new_m.files.contains_key(&pack_path(p)) || !new_m.files.contains_key(&idx_path(p)) {
            bail!("ref map names {p} but the push carries no such pack");
        }
    }
    match old {
        None => {
            if doc.prev.is_some() {
                bail!(
                    "this gateway has not seen this repo yet; retry in a minute or push to another gateway"
                );
            }
        }
        Some((rec, m, old_doc)) => {
            if new.seq <= rec.seq {
                bail!(
                    "stale push: version {} is not newer than {}",
                    new.seq,
                    rec.seq
                );
            }
            if doc.prev.as_deref() != Some(rec.manifest.as_str()) {
                bail!("the repo moved on since you fetched it (fetch, then push again)");
            }
            if !doc.force {
                for p in &old_doc.packs {
                    let (pp, ip) = (pack_path(p), idx_path(p));
                    if new_m.files.get(&pp) != m.files.get(&pp)
                        || new_m.files.get(&ip) != m.files.get(&ip)
                    {
                        bail!("non-forced push drops or changes pack {p}");
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse() {
        let o = "a".repeat(64);
        let u = RepoUrl::parse(&format!("c0mpute://{o}/my-repo.git")).unwrap();
        assert_eq!(u.owner.as_deref(), Some(o.as_str()));
        assert_eq!(u.repo, "my-repo");
        let u = RepoUrl::parse("c0mpute://solo").unwrap();
        assert_eq!(u.owner, None);
        assert_eq!(u.repo, "solo");
        assert!(RepoUrl::parse("c0mpute://short/x").is_err());
        assert!(RepoUrl::parse(&format!("c0mpute://{o}/Bad_Name")).is_err());
        assert!(RepoUrl::parse(&format!("c0mpute://{o}/a/b")).is_err());
        assert_eq!(repo_of_site("git-x"), Some("x"));
        assert_eq!(repo_of_site("blog"), None);
        // The first helper's URL forms.
        let u = RepoUrl::parse("c0mpute://0123456789abcdef0123").unwrap();
        assert_eq!(u.site.as_deref(), Some("0123456789abcdef0123"));
        assert_eq!(
            RepoUrl::parse("c0mpute://git-my-api.git").unwrap().repo,
            "my-api"
        );
        assert_eq!(RepoUrl::parse("c0mpute:my-api").unwrap().repo, "my-api");
    }

    #[test]
    fn private_doc_rules() {
        let mut d = doc(&[], None, false);
        d.v = 2;
        d.head = None;
        d.sealed = true;
        assert!(RepoDoc::from_bytes(&d.to_bytes()).is_ok());
        // A sealed doc must not carry plaintext refs.
        let mut leaky = d.clone();
        leaky.refs.insert("refs/heads/main".into(), "c".repeat(40));
        assert!(RepoDoc::from_bytes(&leaky.to_bytes()).is_err());
        // Version and mode must agree.
        let mut v1_sealed = d.clone();
        v1_sealed.v = 1;
        assert!(RepoDoc::from_bytes(&v1_sealed.to_bytes()).is_err());
        let mut v2_plain = doc(&[], None, false);
        v2_plain.v = 2;
        assert!(RepoDoc::from_bytes(&v2_plain.to_bytes()).is_err());
        // A public doc never serializes the flag (older readers parse it).
        assert!(!String::from_utf8(doc(&[], None, false).to_bytes()).unwrap().contains("sealed"));
        assert!(valid_pack_name(&random_pack_name()));
        assert_ne!(random_pack_name(), random_pack_name());
    }

    #[test]
    fn legacy_layout() {
        let sha = "a".repeat(40);
        let json = format!(r#"{{"head":"refs/heads/main","refs":{{"refs/heads/main":"{sha}"}}}}"#);
        let d = RepoDoc::from_legacy("r", json.as_bytes()).unwrap();
        assert_eq!(d.head.as_deref(), Some("refs/heads/main"));
        assert_eq!(d.refs.get("refs/heads/main"), Some(&sha));
        assert!(d.packs.is_empty() && d.prev.is_none());
        assert!(RepoDoc::from_legacy("r", br#"{"refs":{"HEAD":"zz"}}"#).is_err());
        let mut m = manifest_with(&[]);
        assert!(!is_legacy(&m));
        m.files
            .insert(LEGACY_REFS.into(), file_entry(LEGACY_REFS, b"{}").0);
        m.files
            .insert(LEGACY_BUNDLE.into(), file_entry(LEGACY_BUNDLE, b"x").0);
        assert!(is_legacy(&m));
        m.files
            .insert(DOC_PATH.into(), file_entry(DOC_PATH, b"{}").0);
        assert!(!is_legacy(&m));
    }

    #[test]
    fn ref_names() {
        assert!(valid_ref_name("refs/heads/main"));
        assert!(valid_ref_name("refs/tags/v1.0.0"));
        assert!(!valid_ref_name("HEAD"));
        assert!(!valid_ref_name("refs/heads/../x"));
        assert!(!valid_ref_name("refs/heads/a b"));
        assert!(!valid_ref_name("refs/heads/x.lock"));
        assert!(!valid_ref_name("refs/heads/.hidden"));
    }

    #[test]
    fn push_body_round_trips_and_rejects_garbage() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let m = SiteManifest {
            v: 1,
            name: site_name("r"),
            files: BTreeMap::new(),
        };
        let rec =
            SiteRecord::sign(&kp, &site_name("r"), &Hash::of(&m.to_bytes()), 1, 0, vec![]).unwrap();
        let h = PushHeader {
            record: rec,
            manifest: String::from_utf8(m.to_bytes()).unwrap(),
        };
        let body = encode_push(&h, &[b"one".to_vec(), vec![], vec![7u8; 1000]]);
        let (h2, chunks) = decode_push(&body).unwrap();
        assert_eq!(h2.record, h.record);
        assert_eq!(chunks, vec![b"one".to_vec(), vec![], vec![7u8; 1000]]);
        assert!(decode_push(b"nope").is_err());
        assert!(decode_push(&body[..body.len() - 1]).is_err());
    }

    #[test]
    fn entries_match_their_bytes() {
        let big = vec![3u8; FILE_CHUNK_BYTES + 5];
        let (e, chunks) = file_entry("objects/pack/x.pack", &big);
        assert_eq!(chunks.len(), 2);
        assert!(matches_entry(&e, &big));
        assert!(!matches_entry(&e, &big[1..]));
        let (e0, _) = file_entry("HEAD", b"");
        assert!(matches_entry(&e0, b""));
    }

    fn doc(packs: &[&str], prev: Option<&str>, force: bool) -> RepoDoc {
        RepoDoc {
            v: 1,
            repo: "r".into(),
            head: Some("refs/heads/main".into()),
            refs: BTreeMap::new(),
            packs: packs.iter().map(|s| s.to_string()).collect(),
            prev: prev.map(String::from),
            force,
            pushed_at: 0,
            sealed: false,
        }
    }

    fn manifest_with(packs: &[(&str, &[u8])]) -> SiteManifest {
        let mut files = BTreeMap::new();
        for (p, bytes) in packs {
            files.insert(pack_path(p), file_entry("x.pack", bytes).0);
            files.insert(idx_path(p), file_entry("x.idx", bytes).0);
        }
        SiteManifest {
            v: 1,
            name: site_name("r"),
            files,
        }
    }

    #[test]
    fn update_rules() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let a = format!("pack-{}", "a".repeat(40));
        let b = format!("pack-{}", "b".repeat(40));
        let m1 = manifest_with(&[(&a, b"A")]);
        let r1 = SiteRecord::sign(
            &kp,
            &site_name("r"),
            &Hash::of(&m1.to_bytes()),
            10,
            m1.total_bytes(),
            vec![],
        )
        .unwrap();
        let d1 = doc(&[&a], None, false);
        // First push: no prev.
        check_update(None, &r1, &m1, &d1).unwrap();
        assert!(check_update(None, &r1, &m1, &doc(&[&a], Some("x"), false)).is_err());

        let m2 = manifest_with(&[(&a, b"A"), (&b, b"B")]);
        let r2 = SiteRecord::sign(
            &kp,
            &site_name("r"),
            &Hash::of(&m2.to_bytes()),
            11,
            m2.total_bytes(),
            vec![],
        )
        .unwrap();
        let old = Some((&r1, &m1, &d1));
        check_update(old, &r2, &m2, &doc(&[&a, &b], Some(&r1.manifest), false)).unwrap();
        // Built on a stale view.
        assert!(check_update(old, &r2, &m2, &doc(&[&a, &b], Some("ff"), false)).is_err());
        // Not newer.
        assert!(check_update(old, &r1, &m2, &doc(&[&a, &b], Some(&r1.manifest), false)).is_err());
        // Dropping a pack needs force.
        let m3 = manifest_with(&[(&b, b"B")]);
        let r3 = SiteRecord::sign(
            &kp,
            &site_name("r"),
            &Hash::of(&m3.to_bytes()),
            12,
            m3.total_bytes(),
            vec![],
        )
        .unwrap();
        assert!(check_update(old, &r3, &m3, &doc(&[&b], Some(&r1.manifest), false)).is_err());
        check_update(old, &r3, &m3, &doc(&[&b], Some(&r1.manifest), true)).unwrap();
        // A doc naming a pack the push lacks.
        assert!(check_update(old, &r3, &m3, &doc(&[&a, &b], Some(&r1.manifest), true)).is_err());
    }

    #[test]
    fn dumb_http_files() {
        let mut d = doc(
            &["pack-1111111111111111111111111111111111111111"],
            None,
            false,
        );
        d.refs.insert("refs/heads/main".into(), "c".repeat(40));
        d.refs.insert("refs/tags/v1".into(), "d".repeat(40));
        let mut peeled = BTreeMap::new();
        peeled.insert("d".repeat(40), "c".repeat(40));
        let refs = d.info_refs(&peeled);
        assert_eq!(
            refs,
            format!(
                "{c}\trefs/heads/main\n{d}\trefs/tags/v1\n{c}\trefs/tags/v1^{{}}\n",
                c = "c".repeat(40),
                d = "d".repeat(40)
            )
        );
        assert_eq!(d.head_file(), "ref: refs/heads/main\n");
        assert_eq!(
            d.packs_file(),
            "P pack-1111111111111111111111111111111111111111.pack\n\n"
        );
        let back = RepoDoc::from_bytes(&d.to_bytes()).unwrap();
        assert_eq!(back, d);
    }
}
