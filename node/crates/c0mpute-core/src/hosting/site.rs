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
/// Largest site a node will publish or replicate (with paid preferred hosts):
/// 4 TiB. Striping caps a site at [`super::stripe::MAX_GROUPS`] groups, so
/// past 512 GiB the groups grow (512 MiB each at 4 TiB) instead.
pub const MAX_SITE_BYTES: u64 = 4 * 1024 * GIB;
/// Largest free site (a v1 record, no preferred hosts). Volunteers hold free
/// copies, so a free site may use at most a quarter of what a paid one may.
pub const MAX_FREE_SITE_BYTES: u64 = 64 * GIB;
/// Largest site that is built in memory ([`build_from_dir`]), and the cap for
/// the small sites other features publish (ad creatives, app images, podcast
/// shows, git repos). It was the only site cap before striping.
pub const MAX_SMALL_SITE_BYTES: u64 = GIB;
/// Sites above this size are striped: every holder keeps some chunk groups
/// rather than the whole site (see [`super::stripe`]). At or below it a site
/// is replicated whole, exactly as before, so nodes from 0.3.0 to 0.3.2 (which
/// accept sites up to this size) keep holding and serving it.
pub const STRIPE_ABOVE: u64 = GIB;
/// Largest single file. One file's chunk list lives in one manifest part,
/// which must stay one chunk fetch (under the protocol's 10 MiB response cap).
pub const MAX_FILE_BYTES: u64 = 128 * GIB;
/// Chunks per manifest part (besides [`PART_FILES`]): a part closes before
/// it lists more than this, unless one file alone is bigger.
pub const PART_CHUNKS: usize = 65_536;
const GIB: u64 = 1024 * 1024 * 1024;
/// Most files in one site. Above [`INLINE_FILES`] the manifest is split into
/// parts of [`PART_FILES`], so a gateway loads only the part a request needs.
pub const MAX_FILES: usize = 10_000_000;
/// Sites up to this many files keep the single-chunk (v1) manifest every
/// node understands.
pub const INLINE_FILES: usize = 5_000;
/// Files per manifest part (a part stays a few hundred KiB, well under the
/// chunk protocol's response cap).
pub const PART_FILES: usize = 4_000;
/// Most parts a root may list: file-bounded parts plus chunk-bounded ones.
const MAX_PARTS: usize = MAX_FILES.div_ceil(PART_FILES) + 2 * (MAX_SITE_BYTES as usize / FILE_CHUNK_BYTES).div_ceil(PART_CHUNKS) + 2;
/// Most files a v1 (single-chunk) manifest may list when read.
const MAX_INLINE_FILES: usize = 10_000;
/// Longest site name.
pub const MAX_NAME_CHARS: usize = 63;
/// Most custom domains on one site.
pub const MAX_DOMAINS: usize = 16;

const SIGNING_DOMAIN: &str = "c0mpute-hosting-v1";
/// Records that carry marketplace terms (v2) sign under their own domain, so
/// a v2 signature can never be replayed as a v1 record or the other way round.
const SIGNING_DOMAIN_V2: &str = "c0mpute-hosting-v2";
/// Most preferred (paid) hosts one record can name. Equal to the replica
/// ceiling, so preferred copies alone never push a site over it.
pub const MAX_PREFERRED_HOSTS: usize = 5;

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
        if m.files.len() > MAX_INLINE_FILES {
            bail!("a single-chunk manifest lists more than {MAX_INLINE_FILES} files");
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

fn check_entries<'a>(files: impl Iterator<Item = (&'a String, &'a FileEntry)>) -> Result<()> {
    for (path, f) in files {
        if !valid_path(path) {
            bail!("invalid path in manifest: {path:?}");
        }
        let expected = f.size.div_ceil(FILE_CHUNK_BYTES as u64).max(1) as usize;
        if f.chunks.len() != expected || f.chunks.iter().any(|h| Hash::from_hex(h).is_err()) {
            bail!("file {path:?} has a bad chunk list");
        }
    }
    Ok(())
}

/// One part of a large site's manifest, named in the root by its first path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartRef {
    /// The first (smallest) path in the part.
    pub first: String,
    /// blake3 (hex) of the part chunk.
    pub hash: String,
    pub files: u32,
    pub bytes: u64,
}

/// The root of a large site's manifest (v2): its parts in path order. The
/// record pins this chunk by hash, and it pins every part by hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRoot {
    pub v: u8,
    pub name: String,
    pub files: u64,
    pub bytes: u64,
    pub parts: Vec<PartRef>,
}

/// A run of files of a large site, contiguous in path order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPart {
    pub v: u8,
    pub files: BTreeMap<String, FileEntry>,
}

impl ManifestPart {
    /// Parse a part and check it is the one `r` names.
    pub fn from_bytes(bytes: &[u8], r: &PartRef) -> Result<Self> {
        if Hash::of(bytes).to_hex() != r.hash {
            bail!("manifest part does not match its hash");
        }
        let p: ManifestPart = serde_json::from_slice(bytes).context("manifest part is not JSON")?;
        if p.v != 2 || p.files.len() != r.files as usize || p.files.len() > PART_FILES {
            bail!("manifest part does not match the root");
        }
        if p.files.keys().next().map(String::as_str) != Some(r.first.as_str()) {
            bail!("manifest part starts at the wrong path");
        }
        if p.files.values().map(|f| f.size).sum::<u64>() != r.bytes {
            bail!("manifest part size does not match the root");
        }
        check_entries(p.files.iter())?;
        Ok(p)
    }

    pub fn chunk_hashes(&self) -> impl Iterator<Item = &String> {
        self.files.values().flat_map(|f| f.chunks.iter())
    }
}

/// A site manifest as stored: one chunk (v1), or a root plus parts (v2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Manifest {
    Inline(SiteManifest),
    Parted(ManifestRoot),
}

impl Manifest {
    /// Parse the chunk a record names (either version).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        #[derive(Deserialize)]
        struct V {
            v: u8,
        }
        let v: V = serde_json::from_slice(bytes).context("site manifest is not JSON")?;
        match v.v {
            1 => Ok(Manifest::Inline(SiteManifest::from_bytes(bytes)?)),
            2 => {
                let r: ManifestRoot = serde_json::from_slice(bytes).context("site manifest root")?;
                if r.files as usize > MAX_FILES || r.bytes > MAX_SITE_BYTES || r.parts.is_empty() {
                    bail!("site manifest root out of bounds");
                }
                if r.parts.len() > MAX_PARTS {
                    bail!("site manifest has too many parts");
                }
                if r.parts.iter().map(|p| p.files as u64).sum::<u64>() != r.files
                    || r.parts.iter().map(|p| p.bytes).sum::<u64>() != r.bytes
                {
                    bail!("site manifest parts do not add up");
                }
                if !r.parts.windows(2).all(|w| w[0].first < w[1].first)
                    || r.parts.iter().any(|p| Hash::from_hex(&p.hash).is_err() || !valid_path(&p.first))
                {
                    bail!("site manifest parts are malformed");
                }
                Ok(Manifest::Parted(r))
            }
            n => bail!("unsupported site manifest version {n}"),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Manifest::Inline(m) => &m.name,
            Manifest::Parted(r) => &r.name,
        }
    }

    pub fn total_bytes(&self) -> u64 {
        match self {
            Manifest::Inline(m) => m.total_bytes(),
            Manifest::Parted(r) => r.bytes,
        }
    }

    pub fn file_count(&self) -> u64 {
        match self {
            Manifest::Inline(m) => m.files.len() as u64,
            Manifest::Parted(r) => r.files,
        }
    }

    /// The part that would hold `path` (the last whose first path <= it).
    pub fn part_for(&self, path: &str) -> Option<&PartRef> {
        let Manifest::Parted(r) = self else { return None };
        let i = r.parts.partition_point(|p| p.first.as_str() <= path);
        i.checked_sub(1).map(|i| &r.parts[i])
    }
}

/// The manifest paths a request path may map to, in order: `/` and
/// directories get `index.html`; an extensionless path also tries `.html` and
/// `/index.html`.
pub fn candidates(request: &str) -> Vec<String> {
    let p = request.trim_start_matches('/');
    let p = p.split(['?', '#']).next().unwrap_or("");
    let decoded = percent_decode(p);
    let mut out = Vec::new();
    if decoded.is_empty() || decoded.ends_with('/') {
        out.push(format!("{decoded}index.html"));
    } else {
        out.push(decoded.clone());
        let last = decoded.rsplit('/').next().unwrap_or("");
        if !last.contains('.') {
            out.push(format!("{decoded}.html"));
            out.push(format!("{decoded}/index.html"));
        }
    }
    out
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
    /// v2: hosting nodes (base58 peer ids) the publisher picked, usually from
    /// the d1sks marketplace, and pays per GiB-month. They replicate first;
    /// the normal rendezvous fill still keeps at least three copies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preferred_hosts: Vec<String>,
    /// v2: the agreed price per GiB-month, USD decimal ("0.004").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_per_gib_month: Option<String>,
    /// v2: who pays (hex ed25519 key); the owner when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
    /// ed25519 signature (hex) over [`SiteRecord::signing_bytes`].
    pub sig: String,
}

/// Marketplace terms a v2 record carries.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct HostingTerms {
    pub preferred_hosts: Vec<String>,
    /// Micro-USD per GiB-month.
    pub price_per_gib_month: crate::payments::Micros,
    /// Hex ed25519 key of the payer; `None` = the owner pays.
    pub payer: Option<String>,
}

pub fn valid_peer_id(p: &str) -> bool {
    p.len() <= 64 && p.parse::<c0mpute_net::PeerId>().is_ok()
}

fn valid_key_hex(k: &str) -> bool {
    k.len() == 64 && k.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
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
        let base = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            if self.v >= 2 { SIGNING_DOMAIN_V2 } else { SIGNING_DOMAIN },
            self.site_id,
            self.name,
            self.owner,
            self.manifest,
            self.seq,
            self.size,
            self.domains.join(",")
        );
        if self.v < 2 {
            return base.into_bytes();
        }
        format!(
            "{base}\n{}\n{}\n{}",
            self.preferred_hosts.join(","),
            self.price_per_gib_month.as_deref().unwrap_or(""),
            self.payer.as_deref().unwrap_or("")
        )
        .into_bytes()
    }

    /// The marketplace terms, when this is a v2 record.
    pub fn terms(&self) -> Option<HostingTerms> {
        if self.v < 2 {
            return None;
        }
        Some(HostingTerms {
            preferred_hosts: self.preferred_hosts.clone(),
            price_per_gib_month: self
                .price_per_gib_month
                .as_deref()
                .and_then(|p| crate::payments::parse_usd(p).ok())
                .unwrap_or(0),
            payer: self.payer.clone(),
        })
    }

    /// Who pays for this site's preferred copies: the payer key, else the owner.
    pub fn payer_key(&self) -> &str {
        self.payer.as_deref().unwrap_or(&self.owner)
    }

    /// True when `peer` is one of the record's preferred hosts.
    pub fn prefers(&self, peer: &str) -> bool {
        self.preferred_hosts.iter().any(|p| p == peer)
    }

    /// Build and sign a record with marketplace terms (v2). `terms = None`
    /// signs a plain v1 record, exactly as [`SiteRecord::sign`] does.
    pub fn sign_with_terms(
        keypair: &c0mpute_net::libp2p::identity::Keypair,
        name: &str,
        manifest: &Hash,
        seq: u64,
        size: u64,
        domains: Vec<String>,
        terms: Option<HostingTerms>,
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
            preferred_hosts: Vec::new(),
            price_per_gib_month: None,
            payer: None,
            sig: String::new(),
        };
        if let Some(t) = terms {
            rec.v = 2;
            rec.preferred_hosts = t.preferred_hosts;
            rec.price_per_gib_month = Some(crate::payments::format_usd(t.price_per_gib_month));
            rec.payer = t.payer;
        }
        rec.check_fields()?;
        let sig = keypair
            .sign(&rec.signing_bytes())
            .map_err(|e| anyhow::anyhow!("sign site record: {e}"))?;
        rec.sig = hex::encode(sig);
        Ok(rec)
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
        Self::sign_with_terms(keypair, name, manifest, seq, size, domains, None)
    }

    fn check_fields(&self) -> Result<()> {
        match self.v {
            1 => {
                if !self.preferred_hosts.is_empty() || self.price_per_gib_month.is_some() || self.payer.is_some() {
                    bail!("a v1 site record carries no marketplace terms");
                }
            }
            2 => {
                if self.preferred_hosts.is_empty() || self.preferred_hosts.len() > MAX_PREFERRED_HOSTS {
                    bail!("a v2 site record names 1-{MAX_PREFERRED_HOSTS} preferred hosts");
                }
                if !self.preferred_hosts.iter().all(|p| valid_peer_id(p)) {
                    bail!("preferred host is not a peer id");
                }
                let mut seen = std::collections::HashSet::new();
                if !self.preferred_hosts.iter().all(|p| seen.insert(p)) {
                    bail!("preferred hosts repeat");
                }
                match self.price_per_gib_month.as_deref().map(crate::payments::parse_usd) {
                    // Canonical form only, so every node signs/verifies the same bytes.
                    Some(Ok(m)) if self.price_per_gib_month.as_deref() == Some(crate::payments::format_usd(m).as_str()) => {}
                    _ => bail!("price_per_gib_month must be a canonical USD decimal"),
                }
                if let Some(p) = &self.payer
                    && !valid_key_hex(p)
                {
                    bail!("payer must be a hex ed25519 key");
                }
            }
            v => bail!("unsupported site record version {v}"),
        }
        if !valid_name(&self.name) {
            bail!("site name must be 1-63 of a-z, 0-9 and '-'");
        }
        if self.domains.len() > MAX_DOMAINS || !self.domains.iter().all(|d| valid_domain(d)) {
            bail!("invalid domain list");
        }
        let cap = if self.v >= 2 { MAX_SITE_BYTES } else { MAX_FREE_SITE_BYTES };
        if self.size > cap {
            bail!("site is over {cap} bytes");
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
    // A podcast/RSS feed under the conventional names serves as RSS, so feed
    // readers and browsers treat it as a feed rather than generic XML.
    let file = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if file == "feed.xml" || file == "rss.xml" {
        return "application/rss+xml";
    }
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
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mov" => "video/quicktime",
        "ogv" => "video/ogg",
        "m3u8" => "application/vnd.apple.mpegurl",
        "srt" => "application/x-subrip",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "m4a" => "audio/x-m4a",
        "aac" => "audio/aac",
        "opus" => "audio/opus",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "vtt" => "text/vtt; charset=utf-8",
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
/// A built site's manifest as chunks: the root (what the record pins) and,
/// for a large site, its parts.
pub struct StoredManifest {
    pub root: Vec<u8>,
    pub parts: Vec<Vec<u8>>,
}

impl StoredManifest {
    pub fn hash(&self) -> Hash {
        Hash::of(&self.root)
    }
}

impl BuiltSite {
    /// The stored manifest (see [`SiteManifest::stored`]).
    pub fn stored(&self) -> StoredManifest {
        self.manifest.stored()
    }

    /// The hash a record pins.
    pub fn manifest_hash(&self) -> Hash {
        self.stored().hash()
    }
}

impl SiteManifest {
    /// Encode the manifest: one chunk up to [`INLINE_FILES`] files and
    /// [`STRIPE_ABOVE`] bytes (byte for byte what older nodes read), else a
    /// root plus parts of at most [`PART_FILES`] files and [`PART_CHUNKS`]
    /// chunks, so even a 4 TiB site's chunk lists are fetched a part at a
    /// time.
    pub fn stored(&self) -> StoredManifest {
        let m = self;
        if m.files.len() <= INLINE_FILES && m.total_bytes() <= super::stripe::stripe_above() {
            return StoredManifest {
                root: m.to_bytes(),
                parts: Vec::new(),
            };
        }
        let entries: Vec<(&String, &FileEntry)> = m.files.iter().collect();
        let mut runs: Vec<&[(&String, &FileEntry)]> = Vec::new();
        let (mut start, mut chunks) = (0usize, 0usize);
        for (i, (_, f)) in entries.iter().enumerate() {
            let n = f.chunks.len();
            if i > start && (i - start >= PART_FILES || chunks + n > PART_CHUNKS) {
                runs.push(&entries[start..i]);
                start = i;
                chunks = 0;
            }
            chunks += n;
        }
        if start < entries.len() {
            runs.push(&entries[start..]);
        }
        let mut parts = Vec::new();
        let mut refs = Vec::new();
        for run in runs {
            let part = ManifestPart {
                v: 2,
                files: run.iter().map(|(k, v)| ((*k).clone(), (*v).clone())).collect(),
            };
            let bytes = serde_json::to_vec(&part).expect("part serializes");
            refs.push(PartRef {
                first: run[0].0.clone(),
                hash: Hash::of(&bytes).to_hex(),
                files: run.len() as u32,
                bytes: run.iter().map(|(_, f)| f.size).sum(),
            });
            parts.push(bytes);
        }
        let root = ManifestRoot {
            v: 2,
            name: m.name.clone(),
            files: m.files.len() as u64,
            bytes: m.total_bytes(),
            parts: refs,
        };
        StoredManifest {
            root: serde_json::to_vec(&root).expect("root serializes"),
            parts,
        }
    }
}

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
        if total > MAX_SMALL_SITE_BYTES {
            bail!(
                "site is over {} MiB (publish it with `c0mpute hosting publish`, which scans in place)",
                MAX_SMALL_SITE_BYTES / (1024 * 1024)
            );
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

/// The files `build_from_dir` would publish from `dir`, as sorted
/// (manifest path, absolute path) pairs. Private sites use the same walk.
pub fn list_dir(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut paths = Vec::new();
    walk(dir, dir, &mut paths)?;
    paths.sort();
    Ok(paths)
}

/// Where one chunk of a scanned site lives on the publisher's disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkLoc {
    pub path: PathBuf,
    pub offset: u64,
    pub len: u32,
}

/// A site scanned in place: the manifest, and where each chunk is in the
/// source files. Nothing is copied: the publisher serves chunks straight from
/// the files (see [`super::seed`]), so publishing 50 GiB of video does not
/// need another 50 GiB of local disk.
pub struct ScannedSite {
    pub manifest: SiteManifest,
    pub locations: std::collections::HashMap<Hash, ChunkLoc>,
}

/// Chunk hashes of files already scanned, keyed by absolute path and checked
/// against size and mtime, so publishing the same folder again (to resume, or
/// after changing a few files) reads only what changed.
#[derive(Default, Serialize, Deserialize)]
pub struct ScanCache {
    files: std::collections::HashMap<String, CachedFile>,
}

#[derive(Serialize, Deserialize)]
struct CachedFile {
    size: u64,
    mtime_ns: u128,
    chunks: Vec<String>,
}

impl ScanCache {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("tmp");
        if serde_json::to_vec(self)
            .ok()
            .is_some_and(|b| std::fs::write(&tmp, b).is_ok())
        {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

fn mtime_ns(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// The size limit for a site, as an error that says which limit and what to
/// do about it. `paid` = the site will have preferred (paid) hosts.
pub fn check_site_size(total: u64, paid: bool) -> Result<()> {
    if total > MAX_SITE_BYTES {
        bail!(
            "the site is {}; the most one site can be is {} (paid, --disks) or {} (free). \
             Split the folder into several sites (one --name each).",
            human_bytes(total),
            human_bytes(MAX_SITE_BYTES),
            human_bytes(MAX_FREE_SITE_BYTES)
        );
    }
    if !paid && total > MAX_FREE_SITE_BYTES {
        bail!(
            "the site is {}; a free site can be at most {}, volunteers hold its copies. \
             Add --disks auto to pay hosts (up to {} per site), or split the folder into several sites.",
            human_bytes(total),
            human_bytes(MAX_FREE_SITE_BYTES),
            human_bytes(MAX_SITE_BYTES)
        );
    }
    Ok(())
}

/// Walk `dir` and hash every file in 1 MiB chunks without copying anything.
/// `progress(done, total)` is called as bytes are read. Sizes are checked
/// against [`MAX_SITE_BYTES`] before anything is read.
pub fn scan_dir(
    dir: &Path,
    name: &str,
    cache: &mut ScanCache,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<ScannedSite> {
    use std::io::Read;
    if !valid_name(name) {
        bail!("site name must be 1-63 of a-z, 0-9 and '-'");
    }
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let mut paths = Vec::new();
    walk(dir, dir, &mut paths)?;
    paths.sort();
    if paths.is_empty() {
        bail!("{} has no files to publish", dir.display());
    }
    if paths.len() > MAX_FILES {
        bail!(
            "{} has {} files; a site holds at most {MAX_FILES}",
            dir.display(),
            paths.len()
        );
    }
    let mut metas = Vec::with_capacity(paths.len());
    let mut total = 0u64;
    for (_, abs) in &paths {
        let m = std::fs::metadata(abs).with_context(|| format!("stat {}", abs.display()))?;
        total += m.len();
        metas.push(m);
    }
    check_site_size(total, true)?;
    for ((rel, _), m) in paths.iter().zip(&metas) {
        if m.len() > MAX_FILE_BYTES {
            bail!(
                "{rel} is {}; one file can be at most {}. Split it (for video: `ffmpeg -i in -c copy -f segment ...`).",
                human_bytes(m.len()),
                human_bytes(MAX_FILE_BYTES)
            );
        }
    }
    let mut files = BTreeMap::new();
    let mut locations = std::collections::HashMap::new();
    let mut done = 0u64;
    let mut buf = vec![0u8; FILE_CHUNK_BYTES];
    progress(0, total);
    for ((rel, abs), meta) in paths.into_iter().zip(metas) {
        let size = meta.len();
        let key = abs
            .canonicalize()
            .unwrap_or(abs.clone())
            .to_string_lossy()
            .to_string();
        let mt = mtime_ns(&meta);
        let n = size.div_ceil(FILE_CHUNK_BYTES as u64).max(1) as usize;
        let cached = cache
            .files
            .get(&key)
            .filter(|c| c.size == size && c.mtime_ns == mt && c.chunks.len() == n)
            .map(|c| c.chunks.clone());
        let hashes = match cached {
            Some(h) => {
                done += size;
                progress(done, total);
                h
            }
            None => {
                let mut f =
                    std::fs::File::open(&abs).with_context(|| format!("open {}", abs.display()))?;
                let mut hashes = Vec::with_capacity(n);
                if size == 0 {
                    hashes.push(Hash::of(&[]).to_hex());
                }
                let mut left = size;
                while left > 0 {
                    let want = left.min(FILE_CHUNK_BYTES as u64) as usize;
                    f.read_exact(&mut buf[..want]).with_context(|| {
                        format!("read {} (did it change while publishing?)", abs.display())
                    })?;
                    hashes.push(Hash::of(&buf[..want]).to_hex());
                    left -= want as u64;
                    done += want as u64;
                    progress(done, total);
                }
                cache.files.insert(
                    key,
                    CachedFile {
                        size,
                        mtime_ns: mt,
                        chunks: hashes.clone(),
                    },
                );
                hashes
            }
        };
        for (i, h) in hashes.iter().enumerate() {
            if let Ok(hash) = Hash::from_hex(h) {
                let offset = i as u64 * FILE_CHUNK_BYTES as u64;
                locations.entry(hash).or_insert(ChunkLoc {
                    path: abs.clone(),
                    offset,
                    len: (size.saturating_sub(offset)).min(FILE_CHUNK_BYTES as u64) as u32,
                });
            }
        }
        files.insert(
            rel.clone(),
            FileEntry {
                size,
                content_type: content_type_for(&rel).to_string(),
                chunks: hashes,
            },
        );
    }
    Ok(ScannedSite {
        manifest: SiteManifest {
            v: 1,
            name: name.to_string(),
            files,
        },
        locations,
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
    for c in candidates(request) {
        if let Some((k, v)) = manifest.files.get_key_value(c.as_str()) {
            return Some((k.as_str(), v));
        }
    }
    None
}

/// The directory a request path names (`""` for the root, else ending in
/// `/`), when it names one. Used for the generated listing of a directory
/// that has no `index.html`.
pub fn listing_dir(request: &str) -> Option<String> {
    let p = request.trim_start_matches('/');
    let p = p.split(['?', '#']).next().unwrap_or("");
    let decoded = percent_decode(p);
    (decoded.is_empty() || decoded.ends_with('/')).then_some(decoded)
}

/// Most rows a generated listing shows.
pub const LISTING_MAX_ROWS: usize = 2_000;

/// The immediate children of directory `dir` among `files`: subdirectories
/// (name -> files under it, bytes under it) and files (name, entry).
pub fn dir_children<'a>(
    files: impl IntoIterator<Item = (&'a String, &'a FileEntry)>,
    dir: &str,
) -> (BTreeMap<String, (u64, u64)>, Vec<(String, FileEntry)>) {
    let mut dirs: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut out = Vec::new();
    for (path, entry) in files {
        let Some(rest) = path.strip_prefix(dir) else { continue };
        match rest.split_once('/') {
            Some((d, _)) => {
                let e = dirs.entry(d.to_string()).or_default();
                e.0 += 1;
                e.1 += entry.size;
            }
            None if !rest.is_empty() => out.push((rest.to_string(), entry.clone())),
            None => {}
        }
    }
    (dirs, out)
}

fn listing_href(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$()*,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn listing_esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// A plain HTML index of one directory of a site that has no `index.html`
/// there: links relative to the directory, inline players for audio/video.
pub fn render_listing(
    site_name: &str,
    dir: &str,
    dirs: &BTreeMap<String, (u64, u64)>,
    files: &[(String, FileEntry)],
) -> String {
    let title = if dir.is_empty() {
        site_name.to_string()
    } else {
        format!("{site_name}/{}", dir.trim_end_matches('/'))
    };
    let mut rows = String::new();
    if !dir.is_empty() {
        rows.push_str("<li><a href=\"../\">../</a></li>\n");
    }
    let mut shown = 0usize;
    for (name, (count, bytes)) in dirs {
        if shown >= LISTING_MAX_ROWS {
            break;
        }
        shown += 1;
        rows.push_str(&format!(
            "<li><a href=\"{}/\">{}/</a> <small>{} file{}, {}</small></li>\n",
            listing_href(name),
            listing_esc(name),
            count,
            if *count == 1 { "" } else { "s" },
            human_bytes(*bytes)
        ));
    }
    for (name, e) in files {
        if shown >= LISTING_MAX_ROWS {
            break;
        }
        shown += 1;
        let href = listing_href(name);
        let player = if e.content_type.starts_with("audio/") {
            format!("<audio controls preload=none src=\"{href}\"></audio>")
        } else if e.content_type.starts_with("video/") {
            format!("<video controls preload=none src=\"{href}\"></video>")
        } else {
            String::new()
        };
        rows.push_str(&format!(
            "<li><a href=\"{href}\">{}</a> <small>{}</small>{player}</li>\n",
            listing_esc(name),
            human_bytes(e.size)
        ));
    }
    let total = dirs.len() + files.len();
    let more = if total > shown { format!("<p>{} more not shown.</p>", total - shown) } else { String::new() };
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8>\
<meta name=viewport content=\"width=device-width,initial-scale=1\"><title>{t}</title>\
<style>:root{{color-scheme:light dark}}body{{font:16px/1.5 system-ui,sans-serif;max-width:56rem;margin:0 auto;padding:1rem}}\
ul{{list-style:none;padding:0}}li{{padding:.4rem 0;border-bottom:1px solid #8884;overflow-wrap:anywhere}}\
small{{opacity:.7;margin-left:.5rem}}audio,video{{display:block;width:100%;margin-top:.3rem}}video{{max-height:60vh}}</style>\
</head><body><h1>{t}</h1><ul>\n{rows}</ul>{more}</body></html>",
        t = listing_esc(&title)
    )
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
    fn listing_of_a_folder_of_mp3s() {
        let fe = |size, ct: &str| FileEntry { size, content_type: ct.into(), chunks: vec![] };
        let mut files = BTreeMap::new();
        files.insert("After the Winter/001. Frost & Oar.mp3".to_string(), fe(5_918_737, "audio/mpeg"));
        files.insert("After the Winter/002.mp3".to_string(), fe(100, "audio/mpeg"));
        files.insert("cover.jpg".to_string(), fe(2048, "image/jpeg"));
        assert_eq!(listing_dir("/"), Some(String::new()));
        assert_eq!(listing_dir(""), Some(String::new()));
        assert_eq!(listing_dir("After%20the%20Winter/"), Some("After the Winter/".into()));
        assert_eq!(listing_dir("cover.jpg"), None);

        let (dirs, fs) = dir_children(files.iter(), "");
        assert_eq!(dirs.get("After the Winter"), Some(&(2, 5_918_837)));
        assert_eq!(fs.len(), 1);
        let html = render_listing("brongva", "", &dirs, &fs);
        assert!(html.contains("href=\"After%20the%20Winter/\""), "{html}");
        assert!(html.contains("href=\"cover.jpg\""));

        let (dirs, fs) = dir_children(files.iter(), "After the Winter/");
        assert!(dirs.is_empty());
        let html = render_listing("brongva", "After the Winter/", &dirs, &fs);
        assert!(html.contains("<audio controls preload=none src=\"001.%20Frost%20%26%20Oar.mp3\">"), "{html}");
        assert!(html.contains("001. Frost &amp; Oar.mp3"));
        assert!(html.contains("href=\"../\""));

        let (dirs, fs) = dir_children(files.iter(), "nope/");
        assert!(dirs.is_empty() && fs.is_empty());
    }

    fn synthetic(n: usize) -> BuiltSite {
        let files = (0..n)
            .map(|i| {
                (
                    format!("d{}/f{i:07}.txt", i % 17),
                    FileEntry {
                        size: 3,
                        content_type: "text/plain".into(),
                        chunks: vec![Hash::of(format!("{i}").as_bytes()).to_hex()],
                    },
                )
            })
            .collect();
        BuiltSite {
            manifest: SiteManifest {
                v: 1,
                name: "big".into(),
                files,
            },
            chunks: vec![],
        }
    }

    #[test]
    fn small_sites_keep_the_single_chunk_manifest() {
        let b = synthetic(INLINE_FILES);
        let st = b.stored();
        assert!(st.parts.is_empty());
        assert!(matches!(Manifest::from_bytes(&st.root).unwrap(), Manifest::Inline(_)));
        assert_eq!(st.root, b.manifest.to_bytes(), "byte-identical to what older nodes expect");
    }

    #[test]
    fn large_sites_split_into_parts_every_path_can_find() {
        let b = synthetic(12_345);
        let st = b.stored();
        assert_eq!(st.parts.len(), 12_345usize.div_ceil(PART_FILES));
        let m = Manifest::from_bytes(&st.root).unwrap();
        let Manifest::Parted(r) = &m else { panic!("expected parts") };
        assert_eq!((r.files, r.bytes), (12_345, 3 * 12_345));
        assert_eq!(m.total_bytes(), 3 * 12_345);
        let parts: Vec<ManifestPart> = r.parts.iter().zip(&st.parts).map(|(pr, b)| ManifestPart::from_bytes(b, pr).unwrap()).collect();
        for path in b.manifest.files.keys() {
            let pr = m.part_for(path).unwrap();
            let i = r.parts.iter().position(|x| x == pr).unwrap();
            assert!(parts[i].files.contains_key(path), "{path} routes to the part that holds it");
        }
        assert!(m.part_for("a-before-everything").is_none());
        // A part swapped for another fails its hash check.
        assert!(ManifestPart::from_bytes(&st.parts[1], &r.parts[0]).is_err());
        // A root that lies about its totals is refused.
        let mut bad = r.clone();
        bad.files += 1;
        assert!(Manifest::from_bytes(&serde_json::to_vec(&bad).unwrap()).is_err());
    }

    #[test]
    fn ten_million_files_fit_the_root() {
        // The root for MAX_FILES lists MAX_FILES / PART_FILES parts; it must
        // stay one fetchable chunk (the chunk protocol caps a response at
        // 10 MiB and roughly doubles bytes on the wire).
        let parts = MAX_FILES.div_ceil(PART_FILES);
        let r = ManifestRoot {
            v: 2,
            name: "x".repeat(63),
            files: MAX_FILES as u64,
            bytes: MAX_SITE_BYTES,
            parts: (0..parts)
                .map(|i| PartRef {
                    first: format!("some/fairly/long/directory/name/file-{i:09}.html"),
                    hash: "f".repeat(64),
                    files: PART_FILES as u32,
                    bytes: 0,
                })
                .collect(),
        };
        let n = serde_json::to_vec(&r).unwrap().len();
        assert!(n < 4 * 1024 * 1024, "root is {n} bytes");
    }

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

    fn peer() -> String {
        c0mpute_net::libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
            .to_base58()
    }

    #[test]
    fn v1_records_are_unchanged_and_still_verify() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let rec = SiteRecord::sign(&kp, "old", &Hash::of(b"m"), 1, 10, vec![]).unwrap();
        assert_eq!(rec.v, 1);
        let json = serde_json::to_string(&rec).unwrap();
        // A v1 record serializes with no new keys, so old nodes read it as before.
        assert!(!json.contains("preferred_hosts") && !json.contains("price_per_gib_month") && !json.contains("payer"));
        // And a v1 record as an old node wrote it (no new keys) parses and verifies.
        let back: SiteRecord = serde_json::from_str(&json).unwrap();
        assert!(back.verify());
        assert!(back.terms().is_none());
        assert_eq!(back.payer_key(), back.owner);
    }

    #[test]
    fn v2_records_sign_terms_and_reject_tampering() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let (a, b) = (peer(), peer());
        let terms = HostingTerms {
            preferred_hosts: vec![a.clone(), b.clone()],
            price_per_gib_month: 4_000,
            payer: None,
        };
        let rec = SiteRecord::sign_with_terms(&kp, "paid", &Hash::of(b"m"), 1, 10, vec![], Some(terms.clone())).unwrap();
        assert_eq!(rec.v, 2);
        assert!(rec.verify());
        assert_eq!(rec.price_per_gib_month.as_deref(), Some("0.004"));
        assert_eq!(rec.terms().unwrap(), terms);
        assert!(rec.prefers(&a) && !rec.prefers("nobody"));
        let back: SiteRecord = serde_json::from_slice(&serde_json::to_vec(&rec).unwrap()).unwrap();
        assert!(back.verify());

        let mut t = rec.clone();
        t.preferred_hosts.reverse();
        assert!(!t.verify(), "preferred hosts are signed");
        let mut t = rec.clone();
        t.price_per_gib_month = Some("0.005".into());
        assert!(!t.verify(), "price is signed");
        let mut t = rec.clone();
        t.payer = Some("b".repeat(64));
        assert!(!t.verify(), "payer is signed");
        // Downgrading to v1 (dropping the terms) breaks the signature too.
        let mut t = rec.clone();
        t.v = 1;
        t.preferred_hosts.clear();
        t.price_per_gib_month = None;
        assert!(!t.verify(), "a v2 signature is not a v1 signature");
    }

    #[test]
    fn v2_terms_are_validated() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let sign = |t: HostingTerms| SiteRecord::sign_with_terms(&kp, "x", &Hash::of(b"m"), 1, 10, vec![], Some(t));
        assert!(sign(HostingTerms::default()).is_err(), "no preferred hosts");
        assert!(
            sign(HostingTerms {
                preferred_hosts: vec!["not-a-peer".into()],
                price_per_gib_month: 1,
                payer: None
            })
            .is_err()
        );
        let p = peer();
        assert!(
            sign(HostingTerms {
                preferred_hosts: vec![p.clone(), p.clone()],
                price_per_gib_month: 1,
                payer: None
            })
            .is_err(),
            "duplicates"
        );
        assert!(
            sign(HostingTerms {
                preferred_hosts: (0..6).map(|_| peer()).collect(),
                price_per_gib_month: 1,
                payer: None
            })
            .is_err(),
            "too many"
        );
        assert!(
            sign(HostingTerms {
                preferred_hosts: vec![p.clone()],
                price_per_gib_month: 1,
                payer: Some("zz".into())
            })
            .is_err(),
            "bad payer"
        );
        // A v1 record smuggling terms is rejected.
        let mut r = SiteRecord::sign(&kp, "x", &Hash::of(b"m"), 1, 10, vec![]).unwrap();
        r.preferred_hosts = vec![p];
        assert!(!r.verify());
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

    #[test]
    fn large_sites_split_parts_by_chunks_and_every_part_stays_one_fetch() {
        let mk = |n: usize, seed: u64| -> Vec<String> {
            (0..n).map(|i| Hash::of(&(seed * 1_000_000 + i as u64).to_le_bytes()).to_hex()).collect()
        };
        let mut files = BTreeMap::new();
        for (i, n) in [40_000usize, 40_000, 3, 1].into_iter().enumerate() {
            files.insert(
                format!("movies/{i}.mp4"),
                FileEntry {
                    size: n as u64 * FILE_CHUNK_BYTES as u64 - 5,
                    content_type: "video/mp4".into(),
                    chunks: mk(n, i as u64),
                },
            );
        }
        let m = SiteManifest {
            v: 1,
            name: "big".into(),
            files,
        };
        let st = m.stored();
        // Two files of 40k chunks cannot share a 65,536-chunk part.
        assert_eq!(st.parts.len(), 2);
        let Manifest::Parted(r) = Manifest::from_bytes(&st.root).unwrap() else {
            panic!("a site over STRIPE_ABOVE is parted");
        };
        assert_eq!(r.bytes, m.total_bytes());
        let mut back = BTreeMap::new();
        for (pr, b) in r.parts.iter().zip(&st.parts) {
            assert!(b.len() < 10 * 1024 * 1024, "a part is one fetch: {}", b.len());
            back.extend(ManifestPart::from_bytes(b, pr).unwrap().files);
        }
        assert_eq!(back, m.files);
        // The largest single file still fits one part (one chunk fetch).
        let huge = (MAX_FILE_BYTES / FILE_CHUNK_BYTES as u64) as usize;
        let part = ManifestPart {
            v: 2,
            files: [(
                "x/".repeat(100) + "f.mkv",
                FileEntry {
                    size: MAX_FILE_BYTES,
                    content_type: "video/x-matroska".into(),
                    chunks: vec!["f".repeat(64); huge],
                },
            )]
            .into_iter()
            .collect(),
        };
        assert!(serde_json::to_vec(&part).unwrap().len() < 10 * 1024 * 1024);
    }

    #[test]
    fn size_caps_split_free_and_paid() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let h = Hash::of(b"m");
        assert!(SiteRecord::sign(&kp, "x", &h, 1, MAX_FREE_SITE_BYTES, vec![]).unwrap().verify());
        assert!(SiteRecord::sign(&kp, "x", &h, 1, MAX_FREE_SITE_BYTES + 1, vec![]).is_err(), "free cap");
        let terms = HostingTerms {
            preferred_hosts: vec![peer()],
            price_per_gib_month: 4_000,
            payer: None,
        };
        assert!(
            SiteRecord::sign_with_terms(&kp, "x", &h, 1, MAX_SITE_BYTES, vec![], Some(terms.clone()))
                .unwrap()
                .verify()
        );
        assert!(SiteRecord::sign_with_terms(&kp, "x", &h, 1, MAX_SITE_BYTES + 1, vec![], Some(terms)).is_err());
        let e = check_site_size(70 * GIB, false).unwrap_err().to_string();
        assert!(e.contains("--disks auto"), "{e}");
        assert!(check_site_size(70 * GIB, true).is_ok());
        assert!(check_site_size(300 * GIB, true).is_ok(), "300 GiB is under the 4 TiB cap");
        assert!(check_site_size(MAX_SITE_BYTES + GIB, true).unwrap_err().to_string().contains("Split"));
    }

    #[test]
    fn scanning_hashes_in_place_and_reuses_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        let big: Vec<u8> = (0..(2 * FILE_CHUNK_BYTES + 7)).map(|i| (i % 249) as u8).collect();
        std::fs::write(dir.path().join("v.mp4"), &big).unwrap();
        std::fs::write(dir.path().join("index.html"), b"hi").unwrap();
        std::fs::write(dir.path().join(".secret"), b"no").unwrap();
        let mut cache = ScanCache::default();
        let mut calls = 0;
        let s = scan_dir(dir.path(), "v", &mut cache, &mut |_, _| calls += 1).unwrap();
        assert!(calls > 2);
        let built = build_from_dir(dir.path(), "v").unwrap();
        assert_eq!(s.manifest, built.manifest, "same manifest as the in-memory builder");
        assert_eq!(s.manifest.files["v.mp4"].content_type, "video/mp4");
        let h = Hash::from_hex(&s.manifest.files["v.mp4"].chunks[2]).unwrap();
        let loc = &s.locations[&h];
        assert_eq!((loc.offset, loc.len), (2 * FILE_CHUNK_BYTES as u64, 7));
        // Second scan: nothing read again.
        let mut reads = 0u64;
        let mut last = 0u64;
        let s2 = scan_dir(dir.path(), "v", &mut cache, &mut |d, _| {
            reads += 1;
            last = d;
        })
        .unwrap();
        assert_eq!(s2.manifest, s.manifest);
        assert_eq!(last, big.len() as u64 + 2);
        assert!(reads <= 3, "cached files are not re-read ({reads} progress calls)");
    }

    #[test]
    fn media_types_for_video() {
        assert_eq!(content_type_for("a/b.mkv"), "video/x-matroska");
        assert_eq!(content_type_for("a/b.m4v"), "video/mp4");
        assert_eq!(content_type_for("a/b.MP4"), "video/mp4");
        assert_eq!(content_type_for("a/b.webm"), "video/webm");
        assert_eq!(content_type_for("a/b.mp3"), "audio/mpeg");
    }

    #[test]
    fn feeds_serve_as_rss() {
        assert_eq!(content_type_for("feed.xml"), "application/rss+xml");
        assert_eq!(content_type_for("show/Feed.XML"), "application/rss+xml");
        assert_eq!(content_type_for("podcast.rss"), "application/rss+xml");
        assert_eq!(content_type_for("sitemap.xml"), "application/xml");
        assert_eq!(content_type_for("episodes/a.m4a"), "audio/x-m4a");
    }
}
