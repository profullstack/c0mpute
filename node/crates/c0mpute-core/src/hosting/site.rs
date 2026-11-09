//! What a hosted site is: a signed pointer ([`SiteRecord`]) from an owner's
//! ed25519 key + site name to a content-addressed [`SiteManifest`], whose
//! files are lists of chunks in the c0mpute chunk store.
//!
//! Everything is verifiable from the bytes alone. A site id is derived from
//! the owner key and name, so nobody but the owner can publish a new version
//! of a site; a manifest is pinned by its blake3 hash, and every chunk by its
//! own hash.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};

/// Files are cut into chunks of this size. The chunk protocol carries bytes
/// as a CBOR integer array (about 2x on the wire and slow to decode), so
/// small chunks keep each fetch quick and well under the 10 MiB response cap.
pub const FILE_CHUNK_BYTES: usize = 1024 * 1024;
/// Largest site a node will publish or replicate.
pub const MAX_SITE_BYTES: u64 = 256 * 1024 * 1024;
/// Most files in one site.
pub const MAX_FILES: usize = 10_000;
/// Longest site name.
pub const MAX_NAME_CHARS: usize = 63;
/// Most custom domains on one site.
pub const MAX_DOMAINS: usize = 16;

const SIGNING_DOMAIN: &str = "c0mpute-hosting-v1";

/// One file: its size, MIME type and the chunks that make it up, in order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub size: u64,
    pub content_type: String,
    pub chunks: Vec<String>,
}

/// A site version: every path and its file. Stored as a chunk itself; the
/// record pins it by hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteManifest {
    pub v: u8,
    pub name: String,
    pub files: BTreeMap<String, FileEntry>,
}

impl SiteManifest {
    pub fn total_bytes(&self) -> u64 {
        self.files.values().map(|f| f.size).sum()
    }

    /// Every chunk hash the site needs, manifest excluded.
    pub fn chunk_hashes(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .files
            .values()
            .flat_map(|f| f.chunks.iter().cloned())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("manifest serializes")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let m: SiteManifest = serde_json::from_slice(bytes).context("site manifest is not JSON")?;
        if m.v != 1 {
            bail!("unsupported site manifest version {}", m.v);
        }
        if m.files.len() > MAX_FILES {
            bail!("site has more than {MAX_FILES} files");
        }
        if m.total_bytes() > MAX_SITE_BYTES {
            bail!("site is over {MAX_SITE_BYTES} bytes");
        }
        for (path, f) in &m.files {
            if !valid_path(path) {
                bail!("invalid path in manifest: {path:?}");
            }
            let expected = f.size.div_ceil(FILE_CHUNK_BYTES as u64).max(1) as usize;
            if f.chunks.len() != expected || f.chunks.iter().any(|h| Hash::from_hex(h).is_err()) {
                bail!("file {path:?} has a bad chunk list");
            }
        }
        Ok(m)
    }
}

/// The owner's signed statement "site `name` is now at manifest `manifest`".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteRecord {
    pub v: u8,
    pub site_id: String,
    pub name: String,
    /// Owner's ed25519 public key, hex.
    pub owner: String,
    /// blake3 hash (hex) of the manifest chunk.
    pub manifest: String,
    /// Version: higher wins. Publishers use milliseconds since the epoch.
    pub seq: u64,
    /// Total bytes of the site's files (so a node can decide before fetching).
    pub size: u64,
    /// Custom domains the owner points at this site. A gateway serves one only
    /// after DNS confirms it (see `dns`).
    #[serde(default)]
    pub domains: Vec<String>,
    /// ed25519 signature (hex) over [`SiteRecord::signing_bytes`].
    pub sig: String,
}

/// The id a site with this owner and name always has: 20 hex characters of
/// blake3, so it fits a DNS label and a URL path.
pub fn site_id_for(owner_hex: &str, name: &str) -> String {
    let h = Hash::of(format!("c0mpute-site\n{owner_hex}\n{name}").as_bytes()).to_hex();
    h[..20].to_string()
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

pub fn valid_domain(d: &str) -> bool {
    d.len() <= 253
        && d.contains('.')
        && d.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// A manifest path: relative, `/`-separated, no empty / `.` / `..` segments.
pub fn valid_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 1024
        && !p.starts_with('/')
        && p.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
        && !p.contains('\\')
        && !p.chars().any(|c| c.is_control())
}

impl SiteRecord {
    pub fn signing_bytes(&self) -> Vec<u8> {
        format!(
            "{SIGNING_DOMAIN}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.site_id,
            self.name,
            self.owner,
            self.manifest,
            self.seq,
            self.size,
            self.domains.join(",")
        )
        .into_bytes()
    }

    /// Build and sign a record with the owner's libp2p (ed25519) keypair.
    pub fn sign(
        keypair: &c0mpute_net::libp2p::identity::Keypair,
        name: &str,
        manifest: &Hash,
        seq: u64,
        size: u64,
        domains: Vec<String>,
    ) -> Result<Self> {
        let owner = owner_hex(keypair).context("hosting needs an ed25519 identity")?;
        let mut rec = SiteRecord {
            v: 1,
            site_id: site_id_for(&owner, name),
            name: name.to_string(),
            owner,
            manifest: manifest.to_hex(),
            seq,
            size,
            domains,
            sig: String::new(),
        };
        rec.check_fields()?;
        let sig = keypair
            .sign(&rec.signing_bytes())
            .map_err(|e| anyhow::anyhow!("sign site record: {e}"))?;
        rec.sig = hex::encode(sig);
        Ok(rec)
    }

    fn check_fields(&self) -> Result<()> {
        if self.v != 1 {
            bail!("unsupported site record version {}", self.v);
        }
        if !valid_name(&self.name) {
            bail!("site name must be 1-63 of a-z, 0-9 and '-'");
        }
        if self.domains.len() > MAX_DOMAINS || !self.domains.iter().all(|d| valid_domain(d)) {
            bail!("invalid domain list");
        }
        if self.size > MAX_SITE_BYTES {
            bail!("site is over {MAX_SITE_BYTES} bytes");
        }
        if Hash::from_hex(&self.manifest).is_err() {
            bail!("manifest is not a hash");
        }
        if self.site_id != site_id_for(&self.owner, &self.name) {
            bail!("site id does not match owner and name");
        }
        Ok(())
    }

    /// True when the fields are sane, the id matches, and the owner signed it.
    pub fn verify(&self) -> bool {
        if self.check_fields().is_err() {
            return false;
        }
        let (Ok(key), Ok(sig)) = (hex::decode(&self.owner), hex::decode(&self.sig)) else {
            return false;
        };
        let Ok(public) = c0mpute_net::libp2p::identity::ed25519::PublicKey::try_from_bytes(&key) else {
            return false;
        };
        public.verify(&self.signing_bytes(), &sig)
    }
}

/// Hex ed25519 public key of a keypair, when it is ed25519.
pub fn owner_hex(keypair: &c0mpute_net::libp2p::identity::Keypair) -> Option<String> {
    let ed = keypair.public().try_into_ed25519().ok()?;
    Some(hex::encode(ed.to_bytes()))
}

/// MIME type from a path's extension.
pub fn content_type_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "txt" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "rss" => "application/rss+xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// A site built from a directory: the manifest plus every chunk's bytes.
pub struct BuiltSite {
    pub manifest: SiteManifest,
    pub chunks: Vec<Vec<u8>>,
}

/// Walk `dir` and cut every file into chunks. Hidden files and directories
/// are skipped, except `.well-known`.
pub fn build_from_dir(dir: &Path, name: &str) -> Result<BuiltSite> {
    if !valid_name(name) {
        bail!("site name must be 1-63 of a-z, 0-9 and '-'");
    }
    let mut paths = Vec::new();
    walk(dir, dir, &mut paths)?;
    paths.sort();
    if paths.is_empty() {
        bail!("{} has no files to publish", dir.display());
    }
    if paths.len() > MAX_FILES {
        bail!("{} has more than {MAX_FILES} files", dir.display());
    }
    let mut files = BTreeMap::new();
    let mut chunks = Vec::new();
    let mut total = 0u64;
    for (rel, abs) in paths {
        let bytes = std::fs::read(&abs).with_context(|| format!("read {}", abs.display()))?;
        total += bytes.len() as u64;
        if total > MAX_SITE_BYTES {
            bail!("site is over {} MiB", MAX_SITE_BYTES / (1024 * 1024));
        }
        let mut hashes = Vec::new();
        if bytes.is_empty() {
            hashes.push(Hash::of(&[]).to_hex());
            chunks.push(Vec::new());
        }
        for piece in bytes.chunks(FILE_CHUNK_BYTES) {
            hashes.push(Hash::of(piece).to_hex());
            chunks.push(piece.to_vec());
        }
        files.insert(
            rel.clone(),
            FileEntry {
                size: bytes.len() as u64,
                content_type: content_type_for(&rel).to_string(),
                chunks: hashes,
            },
        );
    }
    Ok(BuiltSite {
        manifest: SiteManifest {
            v: 1,
            name: name.to_string(),
            files,
        },
        chunks,
    })
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let fname = entry.file_name().to_string_lossy().to_string();
        if fname.starts_with('.') && fname != ".well-known" {
            continue;
        }
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            walk(root, &path, out)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .expect("under root")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("/");
            if valid_path(&rel) {
                out.push((rel, path));
            }
        }
    }
    Ok(())
}

/// The manifest path a request path maps to: `/` and directories get
/// `index.html`; an extensionless path also tries `.html` and `/index.html`.
pub fn resolve_path<'a>(manifest: &'a SiteManifest, request: &str) -> Option<(&'a str, &'a FileEntry)> {
    let p = request.trim_start_matches('/');
    let p = p.split(['?', '#']).next().unwrap_or("");
    let decoded = percent_decode(p);
    let mut candidates = Vec::new();
    if decoded.is_empty() || decoded.ends_with('/') {
        candidates.push(format!("{decoded}index.html"));
    } else {
        candidates.push(decoded.clone());
        let last = decoded.rsplit('/').next().unwrap_or("");
        if !last.contains('.') {
            candidates.push(format!("{decoded}.html"));
            candidates.push(format!("{decoded}/index.html"));
        }
    }
    for c in candidates {
        if let Some((k, v)) = manifest.files.get_key_value(c.as_str()) {
            return Some((k.as_str(), v));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_verify_only_when_signed_by_the_owner() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let h = Hash::of(b"manifest");
        let rec = SiteRecord::sign(&kp, "my-site", &h, 7, 100, vec!["example.org".into()]).unwrap();
        assert!(rec.verify());
        assert_eq!(rec.site_id.len(), 20);

        let mut tampered = rec.clone();
        tampered.seq = 8;
        assert!(!tampered.verify());

        // Someone else cannot publish under the same id.
        let other = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let mut stolen = SiteRecord::sign(&other, "my-site", &h, 9, 100, vec![]).unwrap();
        assert_ne!(stolen.site_id, rec.site_id);
        stolen.site_id = rec.site_id.clone();
        assert!(!stolen.verify());
    }

    #[test]
    fn names_domains_paths() {
        assert!(valid_name("blog-2"));
        assert!(!valid_name("Blog"));
        assert!(!valid_name("-x"));
        assert!(!valid_name(""));
        assert!(valid_domain("blog.pit.world"));
        assert!(!valid_domain("localhost"));
        assert!(!valid_domain("a..b"));
        assert!(valid_path("a/b.html"));
        assert!(!valid_path("../etc/passwd"));
        assert!(!valid_path("/abs"));
        assert!(!valid_path("a//b"));
    }

    #[test]
    fn builds_chunks_and_resolves_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<h1>hi</h1>").unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs/index.html"), b"docs").unwrap();
        std::fs::write(dir.path().join("about.html"), b"about").unwrap();
        std::fs::write(dir.path().join(".env"), b"SECRET=1").unwrap();
        let big = vec![7u8; FILE_CHUNK_BYTES + 10];
        std::fs::write(dir.path().join("big.bin"), &big).unwrap();

        let built = build_from_dir(dir.path(), "site").unwrap();
        let m = &built.manifest;
        assert!(!m.files.contains_key(".env"), "hidden files are not published");
        assert_eq!(m.files["big.bin"].chunks.len(), 2);
        assert_eq!(m.files["index.html"].content_type, "text/html; charset=utf-8");
        let again = SiteManifest::from_bytes(&m.to_bytes()).unwrap();
        assert_eq!(&again, m);

        assert_eq!(resolve_path(m, "/").unwrap().0, "index.html");
        assert_eq!(resolve_path(m, "/docs/").unwrap().0, "docs/index.html");
        assert_eq!(resolve_path(m, "/docs").unwrap().0, "docs/index.html");
        assert_eq!(resolve_path(m, "/about").unwrap().0, "about.html");
        assert_eq!(resolve_path(m, "/about.html?x=1").unwrap().0, "about.html");
        assert!(resolve_path(m, "/nope").is_none());
    }
}
