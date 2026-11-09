//! Private storage on the network: the DIP-0012 `private` tier.
//!
//! An object is sealed on the client ([`c0mpute_sealed`]) and the resulting
//! ciphertext blob is published as a hosting site with exactly one file,
//! [`OBJECT_PATH`]. From there the hosting layer does what it does for every
//! site, unchanged: three self-healing copies, any gateway serves it, holders
//! are challenged with random chunk fetches. All of that runs over
//! ciphertext. No node, gateway or log ever sees a key or a plaintext byte:
//! encryption and decryption happen only in this process, on the user's
//! machine.
//!
//! The site record is signed by a per-object key derived from the owner's
//! secret and a random object name, so the network sees an unrelated owner
//! key and a random name per object: it cannot group objects by owner.
//!
//! References:
//!
//! * `c0p1:<site_id>`: readable by the owner and anyone it was shared with
//!   (their storage key unwraps a slot).
//! * `c0p1:<site_id>#k=<key>`: a capability link carrying the object's data
//!   key. Anyone holding it can read. The fragment is never sent anywhere.
//! * `https://<gateway>/p/<site_id>/#k=<key>`: the same, opened in a browser;
//!   the gateway serves a static loader page and the ciphertext, and the
//!   browser decrypts. The fragment never leaves the browser.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use c0mpute_proto::Hash;
use c0mpute_sealed::{DataKey, Kind, Opened, PartRef, SealParams, SiteFile, StoragePublic, StorageSecret};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use crate::hosting::site::{self, BuiltSite, FILE_CHUNK_BYTES, FileEntry, SiteManifest, SiteRecord};
use crate::hosting::{self, Hosting};

/// Reference scheme for private objects.
pub const REF_PREFIX: &str = "c0p1:";
/// The single file a sealed object's hosting site carries.
pub const OBJECT_PATH: &str = "o";
/// Content larger than this is split into separately sealed parts, so every
/// part stays well under the hosting layer's per-site limit after padding.
pub const PART_BYTES: usize = 192 * 1024 * 1024;
/// Where `get` fetches ciphertext from when this machine holds no copy.
pub const DEFAULT_GATEWAY: &str = "https://gateway.c0mpute.com";
/// HKDF label that turns the c0mpute identity seed into the storage root.
pub const STORAGE_LABEL: &[u8] = b"c0mpute-private-storage-root-v1";

/// The storage secret for a c0mpute identity (`c0mpute dm keygen`).
pub fn secret_for(key: &c0mpute_secure_chat::ChatKey) -> StorageSecret {
    StorageSecret::from_seed(&key.derive_subkey(STORAGE_LABEL))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn valid_site_id(s: &str) -> bool {
    s.len() == 20 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ───────────────────────────────────────────────────────────── references

/// A parsed private-object reference.
#[derive(Debug)]
pub struct ObjectRef {
    pub site_id: String,
    /// The data key, when the reference was a capability link.
    pub key: Option<DataKey>,
    /// A path inside a private site (`#path=` on browser links).
    pub path: Option<String>,
}

impl ObjectRef {
    /// Accepts `c0p1:<id>`, `c0p1:<id>#k=<key>`, a bare 20-hex site id, or a
    /// browser link `http(s)://host/p/<id>/...#k=<key>`.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        let (body, frag) = match raw.split_once('#') {
            Some((b, f)) => (b, Some(f)),
            None => (raw, None),
        };
        let site_id = if let Some(id) = body.strip_prefix(REF_PREFIX) {
            id.to_string()
        } else if body.starts_with("http://") || body.starts_with("https://") {
            let after = body.split_once("/p/").map(|(_, a)| a).context("a private link looks like https://<gateway>/p/<id>/#k=...")?;
            after.split('/').next().unwrap_or("").to_string()
        } else {
            body.to_string()
        };
        if !valid_site_id(&site_id) {
            // Do not echo the input: it may carry a #k= key.
            bail!("not a private object reference (expected c0p1:<20 hex>, or a /p/<id>/#k=... link)");
        }
        let mut key = None;
        let mut path = None;
        if let Some(frag) = frag {
            for kv in frag.split('&') {
                match kv.split_once('=') {
                    Some(("k", v)) => key = Some(DataKey::from_link_secret(v).map_err(|_| anyhow::anyhow!("the #k= key in the link is malformed"))?),
                    Some(("path", v)) if !v.is_empty() => path = Some(v.to_string()),
                    _ => {}
                }
            }
        }
        Ok(Self { site_id, key, path })
    }
}

/// `c0p1:<id>`.
pub fn plain_ref(site_id: &str) -> String {
    format!("{REF_PREFIX}{site_id}")
}

/// `c0p1:<id>#k=<key>`: a capability. Treat it like a password.
pub fn capability_ref(site_id: &str, dek: &DataKey) -> Zeroizing<String> {
    Zeroizing::new(format!("{REF_PREFIX}{site_id}#k={}", dek.to_link_secret().as_str()))
}

/// `https://<gateway>/p/<id>/#k=<key>`: opens in a browser, decrypted there.
pub fn browser_link(gateway: &str, site_id: &str, dek: &DataKey) -> Zeroizing<String> {
    Zeroizing::new(format!(
        "{}/p/{site_id}/#k={}",
        gateway.trim_end_matches('/'),
        dek.to_link_secret().as_str()
    ))
}

// ──────────────────────────────────────────────────── hosting adaptation

/// The hosting site that carries a sealed blob: one opaque file.
pub fn built_site(name: &str, blob: &[u8]) -> BuiltSite {
    let mut chunks = Vec::new();
    let mut hashes = Vec::new();
    for piece in blob.chunks(FILE_CHUNK_BYTES) {
        hashes.push(Hash::of(piece).to_hex());
        chunks.push(piece.to_vec());
    }
    let mut files = BTreeMap::new();
    files.insert(
        OBJECT_PATH.to_string(),
        FileEntry {
            size: blob.len() as u64,
            content_type: "application/octet-stream".to_string(),
            chunks: hashes,
        },
    );
    BuiltSite {
        manifest: SiteManifest {
            v: 1,
            name: name.to_string(),
            files,
        },
        chunks,
    }
}

/// Sign the hosting record for object `name` with its per-object key.
pub fn sign_record(owner: &StorageSecret, name: &str, built: &BuiltSite, seq: u64) -> Result<SiteRecord> {
    let mut seed = owner.object_signing_seed(name);
    // ed25519_from_bytes zeroizes the buffer it is given.
    let keypair = c0mpute_net::libp2p::identity::Keypair::ed25519_from_bytes(seed.as_mut())
        .map_err(|e| anyhow::anyhow!("object key: {e}"))?;
    SiteRecord::sign(
        &keypair,
        name,
        &Hash::of(&built.manifest.to_bytes()),
        seq,
        built.manifest.total_bytes(),
        vec![],
    )
}

// ──────────────────────────────────────────────────────────── blob stores

/// Where sealed blobs live. Everything passed in is already ciphertext.
#[async_trait::async_trait]
pub trait BlobStore: Send + Sync {
    /// Publish `blob` as object `name` (a new version if it exists).
    async fn put(&self, owner: &StorageSecret, name: &str, blob: Vec<u8>) -> Result<String>;
    /// Fetch the current blob of a site; `Ok(None)` when it does not exist
    /// (as opposed to an error reaching it).
    async fn try_get(&self, site_id: &str) -> Result<Option<Vec<u8>>>;

    /// Fetch the current blob of a site.
    async fn get(&self, site_id: &str) -> Result<Vec<u8>> {
        self.try_get(site_id).await?.with_context(|| format!("no object {site_id} on the network"))
    }
}

/// Blobs as files in a directory. Offline use and tests.
pub struct DirStore(pub PathBuf);

#[async_trait::async_trait]
impl BlobStore for DirStore {
    async fn put(&self, owner: &StorageSecret, name: &str, blob: Vec<u8>) -> Result<String> {
        let owner_hex = hex::encode(owner.object_signing_key(name).verifying_key().as_bytes());
        let site_id = site::site_id_for(&owner_hex, name);
        tokio::fs::create_dir_all(&self.0).await?;
        let tmp = self.0.join(format!("{site_id}.tmp"));
        tokio::fs::write(&tmp, &blob).await?;
        tokio::fs::rename(&tmp, self.0.join(&site_id)).await?;
        Ok(site_id)
    }

    async fn try_get(&self, site_id: &str) -> Result<Option<Vec<u8>>> {
        if !valid_site_id(site_id) {
            bail!("bad site id");
        }
        match tokio::fs::read(self.0.join(site_id)).await {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// Blobs on the c0mpute network, through the hosting layer.
pub struct NetworkStore {
    hosting: Hosting,
    gateway: String,
    http: reqwest::Client,
    published: Mutex<BTreeMap<String, SiteRecord>>,
    // Keeps the libp2p node alive when this store booted its own.
    _supervisor: Option<crate::Supervisor>,
    // Throwaway identity directory, removed on drop.
    ephemeral_dir: Option<PathBuf>,
}

impl Drop for NetworkStore {
    fn drop(&mut self) {
        if let Some(d) = &self.ephemeral_dir {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

impl NetworkStore {
    /// Join the network as a short-lived client with a fresh, throwaway
    /// libp2p identity, so the peer that announces these objects is not this
    /// machine's usual one (records are signed per object anyway). The
    /// session's objects still share one source address and arrive together.
    pub async fn start(gateway: &str) -> Result<Self> {
        let mut rnd = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut rnd);
        let dir = crate::config::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("private-client")
            .join(hex::encode(rnd));
        std::fs::create_dir_all(&dir)?;
        let supervisor = match crate::Supervisor::boot_with_identity(crate::Config::default(), dir.clone()).await {
            Ok(s) => s,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        let hosting = Hosting::start(supervisor.libp2p.clone(), supervisor.store.clone(), hosting::Options::default()).await?;
        // Let the swarm reach peers before the first gossip.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let mut s = Self::with_hosting(hosting, gateway);
        s._supervisor = Some(supervisor);
        s.ephemeral_dir = Some(dir);
        Ok(s)
    }

    /// Use an existing hosting service (tests, a running node).
    pub fn with_hosting(hosting: Hosting, gateway: &str) -> Self {
        Self {
            hosting,
            gateway: gateway.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .expect("http client"),
            published: Mutex::new(BTreeMap::new()),
            _supervisor: None,
            ephemeral_dir: None,
        }
    }

    pub fn hosting(&self) -> &Hosting {
        &self.hosting
    }

    /// Re-gossip everything published in this session until each object has
    /// [`hosting::TARGET_REPLICAS`] copies or `wait` runs out. Returns the
    /// copy count per site id. `progress` sees each change.
    pub async fn wait_for_copies(&self, wait: Duration, mut progress: impl FnMut(&str, usize)) -> BTreeMap<String, usize> {
        let started = Instant::now();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        loop {
            let records: Vec<SiteRecord> = self.published.lock().await.values().cloned().collect();
            let mut done = true;
            for rec in &records {
                self.hosting.regossip(rec).await;
                let n = self.hosting.holders(&rec.site_id).await.len();
                if counts.get(&rec.site_id) != Some(&n) {
                    progress(&rec.site_id, n);
                    counts.insert(rec.site_id.clone(), n);
                }
                if n < hosting::TARGET_REPLICAS {
                    done = false;
                }
            }
            if done || started.elapsed() >= wait {
                return counts;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for NetworkStore {
    async fn put(&self, owner: &StorageSecret, name: &str, blob: Vec<u8>) -> Result<String> {
        let built = built_site(name, &blob);
        let site_id = {
            let owner_hex = hex::encode(owner.object_signing_key(name).verifying_key().as_bytes());
            site::site_id_for(&owner_hex, name)
        };
        // Strictly newer than any version this session or the network has.
        let mut seq = now_millis();
        if let Some(prev) = self.hosting.record_for(&site_id).await {
            seq = seq.max(prev.seq + 1);
        }
        if let Some(prev) = self.published.lock().await.get(&site_id) {
            seq = seq.max(prev.seq + 1);
        }
        let record = sign_record(owner, name, &built, seq)?;
        self.hosting.publish_site(&built, record.clone()).await?;
        self.published.lock().await.insert(site_id.clone(), record);
        Ok(site_id)
    }

    async fn try_get(&self, site_id: &str) -> Result<Option<Vec<u8>>> {
        if !valid_site_id(site_id) {
            bail!("bad site id");
        }
        // A copy this node knows: read it over libp2p (or locally).
        if let Some(rec) = self.hosting.record_for(site_id).await
            && let Ok(Some((_, body))) = self.hosting.read_file(&rec, OBJECT_PATH).await
        {
            return Ok(Some(body));
        }
        // Otherwise any gateway. It only ever sees and returns ciphertext,
        // and `open` rejects anything it might substitute.
        let url = format!("{}/s/{site_id}/{OBJECT_PATH}", self.gateway);
        fetch_url(&self.http, &url).await
    }
}

/// Blobs fetched read-only from a gateway over HTTP (no libp2p node).
pub struct GatewayStore {
    pub gateway: String,
    http: reqwest::Client,
}

impl GatewayStore {
    pub fn new(gateway: &str) -> Self {
        Self {
            gateway: gateway.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .expect("http client"),
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for GatewayStore {
    async fn put(&self, _owner: &StorageSecret, _name: &str, _blob: Vec<u8>) -> Result<String> {
        bail!("a gateway is read-only; publishing needs a network client")
    }

    async fn try_get(&self, site_id: &str) -> Result<Option<Vec<u8>>> {
        if !valid_site_id(site_id) {
            bail!("bad site id");
        }
        fetch_url(&self.http, &format!("{}/s/{site_id}/{OBJECT_PATH}", self.gateway)).await
    }
}

async fn fetch_url(http: &reqwest::Client, url: &str) -> Result<Option<Vec<u8>>> {
    let resp = http.get(url).send().await.with_context(|| format!("GET {url}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        bail!("GET {url}: HTTP {}", resp.status());
    }
    Ok(Some(resp.bytes().await?.to_vec()))
}

// ─────────────────────────────────────────────────────────── operations

/// What to store.
#[derive(Default)]
pub struct PutOptions {
    pub file_name: Option<String>,
    pub content_type: Option<String>,
    /// Others who can open it with their storage key (the owner always can).
    pub share_with: Vec<StoragePublic>,
}

/// A stored object.
pub struct PutResult {
    pub site_id: String,
    pub dek: DataKey,
    /// Every site id published (the object, plus its parts).
    pub published: Vec<String>,
    pub stored_bytes: u64,
}

fn recipients(owner: &StorageSecret, extra: &[StoragePublic]) -> Vec<StoragePublic> {
    let mut v = vec![owner.public()];
    for r in extra {
        if !v.contains(r) {
            v.push(*r);
        }
    }
    v
}

/// Seal `content` for the owner (and `share_with`) and store it. Content
/// over [`PART_BYTES`] is split into parts, each sealed under its own key;
/// the part keys travel only inside the (sealed) root object.
pub async fn put_bytes(store: &dyn BlobStore, owner: &StorageSecret, content: &[u8], opts: PutOptions) -> Result<PutResult> {
    put_bytes_split(store, owner, content, opts, PART_BYTES).await
}

async fn put_bytes_split(store: &dyn BlobStore, owner: &StorageSecret, content: &[u8], opts: PutOptions, part_bytes: usize) -> Result<PutResult> {
    let rcpts = recipients(owner, &opts.share_with);
    let mut published = Vec::new();
    let mut stored = 0u64;
    let created = now_secs();
    let name = c0mpute_sealed::random_name();
    if content.len() <= part_bytes {
        let sealed = c0mpute_sealed::seal(
            content,
            &SealParams {
                kind: Kind::File,
                name: &name,
                owner,
                file_name: opts.file_name,
                content_type: opts.content_type,
                files: BTreeMap::new(),
                parts: Vec::new(),
                recipients: &rcpts,
                created,
                version: 0,
            },
        )?;
        stored += sealed.blob.len() as u64;
        let id = store.put(owner, &name, sealed.blob).await?;
        published.push(id.clone());
        return Ok(PutResult {
            site_id: id,
            dek: sealed.dek,
            published,
            stored_bytes: stored,
        });
    }
    let parts = put_parts(store, owner, content, part_bytes, created, &mut published, &mut stored).await?;
    let root = c0mpute_sealed::seal(
        &[],
        &SealParams {
            kind: Kind::Multipart,
            name: &name,
            owner,
            file_name: opts.file_name,
            content_type: opts.content_type,
            files: BTreeMap::new(),
            parts,
            recipients: &rcpts,
            created,
            version: 0,
        },
    )?;
    stored += root.blob.len() as u64;
    let id = store.put(owner, &name, root.blob).await?;
    published.push(id.clone());
    Ok(PutResult {
        site_id: id,
        dek: root.dek,
        published,
        stored_bytes: stored,
    })
}

async fn put_parts(
    store: &dyn BlobStore,
    owner: &StorageSecret,
    content: &[u8],
    part_bytes: usize,
    created: u64,
    published: &mut Vec<String>,
    stored: &mut u64,
) -> Result<Vec<PartRef>> {
    let mut parts = Vec::new();
    for piece in content.chunks(part_bytes) {
        let pname = c0mpute_sealed::random_name();
        // No recipient slots: a part opens only with the key in its root.
        let sealed = c0mpute_sealed::seal(
            piece,
            &SealParams {
                kind: Kind::Part,
                name: &pname,
                owner,
                file_name: None,
                content_type: None,
                files: BTreeMap::new(),
                parts: Vec::new(),
                recipients: &[],
                created,
                version: 0,
            },
        )?;
        *stored += sealed.blob.len() as u64;
        let id = store.put(owner, &pname, sealed.blob).await?;
        published.push(id.clone());
        parts.push(PartRef {
            site_id: id,
            key: sealed.dek.to_link_secret().to_string(),
            len: piece.len() as u64,
        });
    }
    Ok(parts)
}

/// A fetched, decrypted object.
pub struct Fetched {
    pub opened: Opened,
    pub dek: DataKey,
}

/// Fetch and decrypt `r`, with its link key or the reader's storage secret.
/// Multipart objects come back reassembled.
pub async fn get(store: &dyn BlobStore, r: &ObjectRef, secret: Option<&StorageSecret>) -> Result<Fetched> {
    let blob = store.get(&r.site_id).await?;
    let (mut opened, dek) = match (&r.key, secret) {
        (Some(k), _) => (c0mpute_sealed::open(&blob, k, Some(&r.site_id))?, k.duplicate()),
        (None, Some(s)) => c0mpute_sealed::open_with_secret(&blob, s, Some(&r.site_id))?,
        (None, None) => bail!("this object needs a key: your storage identity, or a link with #k="),
    };
    check_fresh(&r.site_id, opened.meta.version)?;
    if opened.meta.kind == Kind::Multipart {
        let mut all = Zeroizing::new(Vec::new());
        for p in &opened.meta.parts {
            let pblob = store.get(&p.site_id).await.with_context(|| format!("part {}", p.site_id))?;
            let pkey = DataKey::from_link_secret(&p.key)?;
            let part = c0mpute_sealed::open(&pblob, &pkey, Some(&p.site_id))?;
            if part.meta.kind != Kind::Part || part.content.len() as u64 != p.len {
                bail!("part {} does not match its root", p.site_id);
            }
            all.extend_from_slice(&part.content);
        }
        opened.content = all;
    }
    Ok(Fetched { opened, dek })
}

/// Fetch an object the caller owns, checking that it does: the per-object
/// key derived from the owner's secret and the object's name must be the
/// key the object was signed with.
async fn get_owned(store: &dyn BlobStore, owner: &StorageSecret, site_id: &str) -> Result<(Vec<u8>, Opened, DataKey)> {
    let blob = store.get(site_id).await?;
    let (opened, dek) = c0mpute_sealed::open_with_secret(&blob, owner, Some(site_id))?;
    let mine = hex::encode(owner.object_signing_key(&opened.meta.name).verifying_key().as_bytes());
    if mine != opened.meta.owner {
        bail!("you can read this object but you do not own it, so you cannot change who it is shared with");
    }
    check_fresh(site_id, opened.meta.version)?;
    Ok((blob, opened, dek))
}

/// Add readers. Rewraps the key only; the ciphertext and every part stay
/// as they are, so this republishes one small header change.
pub async fn share(store: &dyn BlobStore, owner: &StorageSecret, site_id: &str, add: &[StoragePublic]) -> Result<Vec<StoragePublic>> {
    let (blob, opened, dek) = get_owned(store, owner, site_id).await?;
    let mut roster = c0mpute_sealed::roster(&blob, owner)?;
    for r in add {
        if !roster.contains(r) {
            roster.push(*r);
        }
    }
    let new = c0mpute_sealed::rewrap(&blob, &dek, owner, &roster)?;
    store.put(owner, &opened.meta.name, new).await?;
    Ok(roster)
}

/// Remove readers for real: re-seal everything (root and parts) under new
/// keys and publish the new version, so the old keys open nothing current.
/// Anyone removed may still have kept a copy of what they could read before.
pub async fn revoke(store: &dyn BlobStore, owner: &StorageSecret, site_id: &str, remove: &[StoragePublic]) -> Result<(Vec<StoragePublic>, PutResult)> {
    let (blob, opened, _old) = get_owned(store, owner, site_id).await?;
    let roster: Vec<StoragePublic> = c0mpute_sealed::roster(&blob, owner)?
        .into_iter()
        .filter(|r| !remove.contains(r))
        .collect();
    let meta = opened.meta.clone();
    let mut published = Vec::new();
    let mut stored = 0;
    let (content, parts) = if meta.kind == Kind::Multipart {
        let full = get(store, &ObjectRef { site_id: site_id.to_string(), key: None, path: None }, Some(owner)).await?;
        // New parts under new names and keys. The old part sites are
        // tombstoned only after the new root is published, so a failure
        // half way never leaves the object unreadable.
        let parts = put_parts(store, owner, &full.opened.content, PART_BYTES, meta.created, &mut published, &mut stored).await?;
        (Zeroizing::new(Vec::new()), parts)
    } else {
        (opened.content, Vec::new())
    };
    let sealed = c0mpute_sealed::seal(
        &content,
        &SealParams {
            kind: meta.kind,
            name: &meta.name,
            owner,
            file_name: meta.file_name.clone(),
            content_type: meta.content_type.clone(),
            files: meta.files.clone(),
            parts,
            recipients: &roster,
            created: meta.created,
            version: next_version(meta.version),
        },
    )?;
    stored += sealed.blob.len() as u64;
    let id = store.put(owner, &meta.name, sealed.blob).await?;
    published.push(id.clone());
    note_version(&id, next_version(meta.version));
    for p in &meta.parts {
        if let Ok(pblob) = store.get(&p.site_id).await
            && let Ok(pkey) = DataKey::from_link_secret(&p.key)
            && let Ok(part) = c0mpute_sealed::open(&pblob, &pkey, Some(&p.site_id))
        {
            let _ = store.put(owner, &part.meta.name, tombstone()).await;
        }
    }
    Ok((
        roster,
        PutResult {
            site_id: id,
            dek: sealed.dek,
            published,
            stored_bytes: stored,
        },
    ))
}

/// What a deleted object is replaced with: 4 KiB of random bytes, which no
/// key opens. Publishing it makes holders garbage-collect the ciphertext.
fn tombstone() -> Vec<u8> {
    let mut b = vec![0u8; 4096];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut b);
    b
}

/// Delete an object (and its parts): publish a tombstone version.
pub async fn delete(store: &dyn BlobStore, owner: &StorageSecret, site_id: &str) -> Result<Vec<String>> {
    let (_, opened, _) = get_owned(store, owner, site_id).await?;
    let mut done = Vec::new();
    for p in &opened.meta.parts {
        let pblob = store.get(&p.site_id).await?;
        let pkey = DataKey::from_link_secret(&p.key)?;
        let part = c0mpute_sealed::open(&pblob, &pkey, Some(&p.site_id))?;
        done.push(store.put(owner, &part.meta.name, tombstone()).await?);
    }
    done.push(store.put(owner, &opened.meta.name, tombstone()).await?);
    Ok(done)
}

/// Pack a directory into one private site object's content + file table.
pub fn pack_site(dir: &Path) -> Result<(Vec<u8>, BTreeMap<String, SiteFile>)> {
    let paths = site::list_dir(dir)?;
    if paths.is_empty() {
        bail!("{} has no files to publish", dir.display());
    }
    if paths.len() > site::MAX_FILES {
        bail!("{} has more than {} files", dir.display(), site::MAX_FILES);
    }
    let mut content = Vec::new();
    let mut files = BTreeMap::new();
    for (rel, abs) in paths {
        let bytes = std::fs::read(&abs).with_context(|| format!("read {}", abs.display()))?;
        files.insert(
            rel.clone(),
            SiteFile {
                offset: content.len() as u64,
                len: bytes.len() as u64,
                content_type: site::content_type_for(&rel).to_string(),
            },
        );
        content.extend_from_slice(&bytes);
        if content.len() > PART_BYTES {
            bail!("a private site can be at most {} MiB", PART_BYTES / (1024 * 1024));
        }
    }
    Ok((content, files))
}

/// Publish (or update) the private site `label` from `dir`. The network name
/// is derived from the owner's secret and the label, so republishing updates
/// the same object; the data key is kept across versions (the link stays
/// valid) unless `rekey`, which invalidates every link given out so far.
pub async fn put_site(store: &dyn BlobStore, owner: &StorageSecret, dir: &Path, label: &str, rekey: bool) -> Result<PutResult> {
    let (content, files) = pack_site(dir)?;
    let name = owner.named_object(label);
    let site_id = site::site_id_for(&hex::encode(owner.object_signing_key(&name).verifying_key().as_bytes()), &name);
    // The previous version, fully verified (signature, site id, owner,
    // freshness): its key is reused so links keep working, and its roster
    // so shares survive. Never take a key from an unverified blob, or a
    // gateway could choose it.
    let (previous, roster, prev_version) = match store.try_get(&site_id).await? {
        None => (None, vec![owner.public()], 0),
        Some(blob) => {
            let (opened, dek) = c0mpute_sealed::open_with_secret(&blob, owner, Some(&site_id))
                .with_context(|| format!("private site {label}: the published version does not verify; refusing to update it"))?;
            let mine = hex::encode(owner.object_signing_key(&name).verifying_key().as_bytes());
            if mine != opened.meta.owner || opened.meta.kind != Kind::Site {
                bail!("private site {label}: the published object is not yours");
            }
            check_fresh(&site_id, opened.meta.version)?;
            let roster = c0mpute_sealed::roster(&blob, owner).unwrap_or_else(|_| vec![owner.public()]);
            (if rekey { None } else { Some(dek) }, roster, opened.meta.version)
        }
    };
    let version = next_version(prev_version);
    let params = SealParams {
        kind: Kind::Site,
        name: &name,
        owner,
        file_name: Some(label.to_string()),
        content_type: None,
        files,
        parts: Vec::new(),
        recipients: &roster,
        created: now_secs(),
        version,
    };
    let sealed = match previous {
        Some(dek) => c0mpute_sealed::seal_with_key(&content, &params, dek)?,
        None => c0mpute_sealed::seal(&content, &params)?,
    };
    let stored = sealed.blob.len() as u64;
    let id = store.put(owner, &name, sealed.blob).await?;
    note_version(&id, version);
    Ok(PutResult {
        site_id: id.clone(),
        dek: sealed.dek,
        published: vec![id],
        stored_bytes: stored,
    })
}

/// Write a fetched private site out as a directory tree.
pub fn unpack_site(opened: &Opened, out: &Path) -> Result<usize> {
    let mut n = 0;
    for (path, f) in &opened.meta.files {
        if !site::valid_path(path) {
            bail!("refusing unsafe path {path:?} in private site");
        }
        let dest = out.join(path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let start = f.offset as usize;
        std::fs::write(&dest, &opened.content[start..start + f.len as usize])?;
        n += 1;
    }
    Ok(n)
}

// ──────────────────────────────────────────────────────── encrypted volume

/// One file in a volume.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VolFile {
    pub site_id: String,
    /// base64url DEK of the file's object (lives only in the sealed index).
    pub key: String,
    pub size: u64,
    /// Unix seconds.
    pub mtime: u64,
}

impl std::fmt::Debug for VolFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VolFile").field("site_id", &self.site_id).field("key", &"<redacted>").field("size", &self.size).finish()
    }
}

/// A volume's directory: sealed as one [`Kind::Volume`] object.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VolumeIndex {
    pub files: BTreeMap<String, VolFile>,
    pub dirs: std::collections::BTreeSet<String>,
}

/// An encrypted volume (`c0mpute storage mount`): files are private
/// objects, and the directory is one more private object whose network name
/// is derived from the owner's secret and the volume label. Nodes see a set
/// of unrelated objects with random names; they cannot tell which belong to
/// one volume, what the files are called, or how the tree is shaped.
pub struct Volume {
    store: std::sync::Arc<dyn BlobStore>,
    owner: std::sync::Arc<StorageSecret>,
    name: String,
    version: u64,
    pub index: VolumeIndex,
}

fn volume_path_ok(p: &str) -> bool {
    p.is_empty() || site::valid_path(p)
}

impl Volume {
    /// Open (or start) the volume `label`. A volume whose directory exists
    /// but cannot be fetched or opened is an error, never silently empty, so
    /// a network blip cannot make the next write overwrite a real volume.
    pub async fn open(store: std::sync::Arc<dyn BlobStore>, owner: std::sync::Arc<StorageSecret>, label: &str) -> Result<Self> {
        let name = owner.named_object(&format!("volume:{label}"));
        let site_id = site::site_id_for(&hex::encode(owner.object_signing_key(&name).verifying_key().as_bytes()), &name);
        let mut version = 0;
        let index = match store.try_get(&site_id).await? {
            None => VolumeIndex::default(),
            Some(blob) => {
                let (opened, _) = c0mpute_sealed::open_with_secret(&blob, &owner, Some(&site_id))
                    .with_context(|| format!("volume {label}: directory does not open with this identity"))?;
                if opened.meta.kind != Kind::Volume {
                    bail!("volume {label}: directory object is a {:?}", opened.meta.kind);
                }
                check_fresh(&site_id, opened.meta.version)?;
                version = opened.meta.version;
                serde_json::from_slice(&opened.content).context("volume directory is malformed")?
            }
        };
        Ok(Self { store, owner, name, version, index })
    }

    pub fn site_id(&self) -> String {
        site::site_id_for(&hex::encode(self.owner.object_signing_key(&self.name).verifying_key().as_bytes()), &self.name)
    }

    async fn save(&mut self) -> Result<()> {
        self.version = next_version(self.version);
        let content = Zeroizing::new(serde_json::to_vec(&self.index)?);
        let sealed = c0mpute_sealed::seal(
            &content,
            &SealParams {
                kind: Kind::Volume,
                name: &self.name,
                owner: &self.owner,
                file_name: None,
                content_type: None,
                files: BTreeMap::new(),
                parts: Vec::new(),
                recipients: &[self.owner.public()],
                created: now_secs(),
                version: self.version,
            },
        )?;
        let id = self.store.put(&self.owner, &self.name, sealed.blob).await?;
        note_version(&id, self.version);
        Ok(())
    }

    /// Read a whole file.
    pub async fn read(&self, path: &str) -> Result<Zeroizing<Vec<u8>>> {
        let f = self.index.files.get(path).with_context(|| format!("no file {path}"))?;
        let r = ObjectRef {
            site_id: f.site_id.clone(),
            key: Some(DataKey::from_link_secret(&f.key)?),
            path: None,
        };
        Ok(get(self.store.as_ref(), &r, None).await?.opened.content)
    }

    /// Write a whole file: a new object under a fresh key, then the index.
    /// The previous version's object is tombstoned afterwards.
    pub async fn write(&mut self, path: &str, content: &[u8]) -> Result<()> {
        if !volume_path_ok(path) || path.is_empty() {
            bail!("invalid path {path:?}");
        }
        let put = put_bytes(self.store.as_ref(), &self.owner, content, PutOptions::default()).await?;
        let old = self.index.files.insert(
            path.to_string(),
            VolFile {
                site_id: put.site_id,
                key: put.dek.to_link_secret().to_string(),
                size: content.len() as u64,
                mtime: now_secs(),
            },
        );
        self.save().await?;
        if let Some(old) = old {
            let _ = delete(self.store.as_ref(), &self.owner, &old.site_id).await;
        }
        Ok(())
    }

    pub async fn remove(&mut self, path: &str) -> Result<()> {
        let old = self.index.files.remove(path).with_context(|| format!("no file {path}"))?;
        self.save().await?;
        let _ = delete(self.store.as_ref(), &self.owner, &old.site_id).await;
        Ok(())
    }

    pub async fn mkdir(&mut self, path: &str) -> Result<()> {
        if !volume_path_ok(path) || path.is_empty() {
            bail!("invalid path {path:?}");
        }
        self.index.dirs.insert(path.to_string());
        self.save().await
    }

    pub async fn rmdir(&mut self, path: &str) -> Result<()> {
        if self.children(path).next().is_some() {
            bail!("directory not empty");
        }
        self.index.dirs.remove(path);
        self.save().await
    }

    /// Rename a file or directory (index only: no data moves).
    pub async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        if !volume_path_ok(to) || to.is_empty() {
            bail!("invalid path {to:?}");
        }
        let replaced = if let Some(f) = self.index.files.remove(from) {
            self.index.files.insert(to.to_string(), f)
        } else if self.is_dir(from) {
            let prefix = format!("{from}/");
            let moved: Vec<String> = self.index.files.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
            for k in moved {
                let f = self.index.files.remove(&k).expect("listed");
                self.index.files.insert(format!("{to}/{}", &k[prefix.len()..]), f);
            }
            let dirs: Vec<String> = self.index.dirs.iter().filter(|d| *d == from || d.starts_with(&prefix)).cloned().collect();
            for d in dirs {
                self.index.dirs.remove(&d);
                self.index.dirs.insert(format!("{to}{}", &d[from.len()..]));
            }
            None
        } else {
            bail!("no such file {from}");
        };
        self.save().await?;
        if let Some(old) = replaced {
            let _ = delete(self.store.as_ref(), &self.owner, &old.site_id).await;
        }
        Ok(())
    }

    pub fn is_dir(&self, path: &str) -> bool {
        path.is_empty() || self.index.dirs.contains(path) || {
            let prefix = format!("{path}/");
            self.index.files.keys().any(|k| k.starts_with(&prefix)) || self.index.dirs.iter().any(|d| d.starts_with(&prefix))
        }
    }

    /// Direct children of `dir` as (name, is_dir).
    pub fn children<'a>(&'a self, dir: &str) -> impl Iterator<Item = (String, bool)> + 'a {
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        let mut out: BTreeMap<String, bool> = BTreeMap::new();
        for k in self.index.files.keys().chain(self.index.dirs.iter()) {
            if let Some(rest) = k.strip_prefix(&prefix)
                && !rest.is_empty()
            {
                match rest.split_once('/') {
                    Some((first, _)) => {
                        out.insert(first.to_string(), true);
                    }
                    None => {
                        let is_dir = self.index.dirs.contains(k);
                        out.entry(rest.to_string()).or_insert(is_dir);
                    }
                }
            }
        }
        out.into_iter()
    }
}

// ─────────────────────────────────────────────────── rollback protection

/// The newest version of each object this machine has seen or published.
/// Every object carries a signed version; a node or gateway can still
/// replay an *older* signed version (say, from before a revocation), and
/// only the reader's memory can catch that. The CLI installs one backed by
/// an encrypted file; without one, nothing is checked.
pub struct Freshness {
    seen: std::sync::Mutex<BTreeMap<String, u64>>,
    save: Box<dyn Fn(&BTreeMap<String, u64>) + Send + Sync>,
}

static FRESHNESS: std::sync::OnceLock<Freshness> = std::sync::OnceLock::new();

impl Freshness {
    /// Install the process-wide tracker (first call wins).
    pub fn install(seen: BTreeMap<String, u64>, save: impl Fn(&BTreeMap<String, u64>) + Send + Sync + 'static) {
        let _ = FRESHNESS.set(Freshness {
            seen: std::sync::Mutex::new(seen),
            save: Box::new(save),
        });
    }

    /// Install one persisted next to the local index, encrypted under the
    /// owner's local key.
    pub fn install_for(owner: &StorageSecret) {
        let path = crate::config::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("private-storage-seen.idx");
        let seen = LocalIndex::load(&path, owner).map(|i| i.seen).unwrap_or_default();
        let key = LocalIndex::cipher_key(owner);
        Freshness::install(seen, move |map| {
            let idx = LocalIndex {
                seen: map.clone(),
                ..Default::default()
            };
            let _ = idx.save_with_key(&path, &key);
        });
    }
}

fn check_fresh(site_id: &str, version: u64) -> Result<()> {
    let Some(f) = FRESHNESS.get() else { return Ok(()) };
    let mut seen = f.seen.lock().expect("freshness lock");
    match seen.get(site_id) {
        Some(v) if version < *v => bail!(
            "object {site_id} was served at an older version than this machine has already seen \
             (a node or gateway is replaying it); refusing"
        ),
        Some(v) if version == *v => Ok(()),
        _ => {
            seen.insert(site_id.to_string(), version);
            (f.save)(&seen);
            Ok(())
        }
    }
}

fn note_version(site_id: &str, version: u64) {
    let _ = check_fresh(site_id, version);
}

fn next_version(prev: u64) -> u64 {
    c0mpute_sealed::now_version().max(prev.saturating_add(1))
}

// ────────────────────────────────────────────────────────── local index

/// The owner's list of its objects, on its own disk only, encrypted under
/// a key derived from its storage secret. The network never sees it.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct LocalIndex {
    pub objects: BTreeMap<String, IndexEntry>,
    /// Newest version seen per object (rollback protection).
    #[serde(default)]
    pub seen: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IndexEntry {
    pub file_name: Option<String>,
    pub size: u64,
    pub kind: Kind,
    pub created: u64,
    #[serde(default)]
    pub shared_with: usize,
}

impl LocalIndex {
    fn cipher_key(owner: &StorageSecret) -> Zeroizing<[u8; 32]> {
        let mut k = Zeroizing::new([0u8; 32]);
        hkdf::Hkdf::<sha2::Sha256>::new(None, owner.local_index_key())
            .expand(b"c0mpute-private-local-index-file-v1", k.as_mut())
            .expect("32 bytes");
        k
    }

    fn cipher(owner: &StorageSecret) -> aes_gcm::Aes256Gcm {
        Self::cipher_from(&Self::cipher_key(owner))
    }

    fn cipher_from(key: &[u8; 32]) -> aes_gcm::Aes256Gcm {
        use aes_gcm::KeyInit;
        aes_gcm::Aes256Gcm::new_from_slice(key).expect("32-byte key")
    }

    pub fn load(path: &Path, owner: &StorageSecret) -> Result<Self> {
        use aes_gcm::aead::Aead;
        let Ok(raw) = std::fs::read(path) else {
            return Ok(Self::default());
        };
        if raw.len() < 12 {
            bail!("local index {} is corrupt", path.display());
        }
        let plain = Self::cipher(owner)
            .decrypt(aes_gcm::Nonce::from_slice(&raw[..12]), &raw[12..])
            .map_err(|_| anyhow::anyhow!("local index {} does not open with this identity", path.display()))?;
        Ok(serde_json::from_slice(&plain)?)
    }

    pub fn save(&self, path: &Path, owner: &StorageSecret) -> Result<()> {
        self.save_with_key(path, &Self::cipher_key(owner))
    }

    fn save_with_key(&self, path: &Path, key: &[u8; 32]) -> Result<()> {
        use aes_gcm::aead::Aead;
        let plain = Zeroizing::new(serde_json::to_vec(self)?);
        let mut nonce = [0u8; 12];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
        let ct = Self::cipher_from(key)
            .encrypt(aes_gcm::Nonce::from_slice(&nonce), plain.as_slice())
            .map_err(|_| anyhow::anyhow!("encrypt local index"))?;
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// `<config_dir>/private-storage.idx`.
    pub fn default_path() -> PathBuf {
        crate::config::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("private-storage.idx")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, DirStore) {
        let d = tempfile::tempdir().unwrap();
        let s = DirStore(d.path().join("blobs"));
        (d, s)
    }

    #[test]
    fn site_id_derivation_matches_hosting() {
        for (o, n) in [("ab".repeat(32), "x"), ("00".repeat(32), "some-name")] {
            assert_eq!(c0mpute_sealed::site_id_for(&o, n), site::site_id_for(&o, n));
        }
    }

    #[test]
    fn refs_parse() {
        let id = "0123456789abcdef0123";
        assert_eq!(ObjectRef::parse(&format!("c0p1:{id}")).unwrap().site_id, id);
        assert_eq!(ObjectRef::parse(id).unwrap().site_id, id);
        let owner = StorageSecret::generate();
        let sealed = c0mpute_sealed::seal(
            b"x",
            &SealParams {
                kind: Kind::File,
                name: "n",
                owner: &owner,
                file_name: None,
                content_type: None,
                files: BTreeMap::new(),
                parts: vec![],
                recipients: &[],
                created: 0,
                version: 0,
            },
        )
        .unwrap();
        let link = browser_link("https://gw.example/", id, &sealed.dek);
        let r = ObjectRef::parse(&link).unwrap();
        assert_eq!(r.site_id, id);
        assert!(r.key.is_some());
        let r = ObjectRef::parse(&capability_ref(id, &sealed.dek)).unwrap();
        assert!(r.key.is_some());
        assert!(ObjectRef::parse("c0p1:nothex").is_err());
        assert!(ObjectRef::parse("https://gw/s/abc").is_err());
    }

    #[test]
    fn sealed_site_is_a_valid_single_file_hosting_site() {
        let owner = StorageSecret::generate();
        let blob = vec![7u8; 3 * FILE_CHUNK_BYTES + 5];
        let built = built_site("abc", &blob);
        let m = SiteManifest::from_bytes(&built.manifest.to_bytes()).unwrap();
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[OBJECT_PATH].chunks.len(), 4);
        let rec = sign_record(&owner, "abc", &built, 1).unwrap();
        assert!(rec.verify());
        let owner_hex = hex::encode(owner.object_signing_key("abc").verifying_key().as_bytes());
        assert_eq!(rec.owner, owner_hex);
        assert_eq!(rec.site_id, site::site_id_for(&owner_hex, "abc"));
    }

    #[tokio::test]
    async fn put_get_share_revoke_delete() {
        let (_d, s) = store();
        let alice = StorageSecret::generate();
        let bob = StorageSecret::generate();
        let data = b"quarterly numbers, do not leak".repeat(100);
        let put = put_bytes(&s, &alice, &data, PutOptions { file_name: Some("q3.txt".into()), ..Default::default() }).await.unwrap();
        let r = ObjectRef::parse(&plain_ref(&put.site_id)).unwrap();
        let got = get(&s, &r, Some(&alice)).await.unwrap();
        assert_eq!(&*got.opened.content, &data);
        assert_eq!(got.opened.meta.file_name.as_deref(), Some("q3.txt"));
        // The stored bytes are ciphertext.
        let raw = s.get(&put.site_id).await.unwrap();
        assert!(!raw.windows(9).any(|w| w == b"quarterly"));
        assert!(get(&s, &r, Some(&bob)).await.is_err());

        // Share with bob; bob cannot reshare (he does not own it).
        let roster = share(&s, &alice, &put.site_id, &[bob.public()]).await.unwrap();
        assert_eq!(roster, vec![alice.public(), bob.public()]);
        let as_bob = get(&s, &r, Some(&bob)).await.unwrap();
        assert_eq!(&*as_bob.opened.content, &data);
        assert!(share(&s, &bob, &put.site_id, &[]).await.is_err());

        // Revoke bob: new keys, bob's old key and slot open nothing.
        let (roster, re) = revoke(&s, &alice, &put.site_id, &[bob.public()]).await.unwrap();
        assert_eq!(roster, vec![alice.public()]);
        assert_eq!(re.site_id, put.site_id);
        assert!(get(&s, &r, Some(&bob)).await.is_err());
        let old_link = ObjectRef { site_id: put.site_id.clone(), key: Some(as_bob.dek.duplicate()), path: None };
        assert!(get(&s, &old_link, None).await.is_err());
        assert_eq!(&*get(&s, &r, Some(&alice)).await.unwrap().opened.content, &data);

        // A capability link works without any identity.
        let cap = ObjectRef::parse(&capability_ref(&put.site_id, &re.dek)).unwrap();
        assert_eq!(&*get(&s, &cap, None).await.unwrap().opened.content, &data);

        delete(&s, &alice, &put.site_id).await.unwrap();
        assert!(get(&s, &r, Some(&alice)).await.is_err());
    }

    #[tokio::test]
    async fn private_sites_keep_their_link_across_versions() {
        let (d, s) = store();
        let alice = StorageSecret::generate();
        let site_dir = d.path().join("site");
        std::fs::create_dir_all(site_dir.join("css")).unwrap();
        std::fs::write(site_dir.join("index.html"), b"<link href=css/a.css><h1>v1</h1>").unwrap();
        std::fs::write(site_dir.join("css/a.css"), b"h1{color:red}").unwrap();
        let v1 = put_site(&s, &alice, &site_dir, "family-photos", false).await.unwrap();
        let link = browser_link("https://gw", &v1.site_id, &v1.dek);
        std::fs::write(site_dir.join("index.html"), b"<h1>v2</h1>").unwrap();
        let v2 = put_site(&s, &alice, &site_dir, "family-photos", false).await.unwrap();
        assert_eq!(v2.site_id, v1.site_id);
        let got = get(&s, &ObjectRef::parse(&link).unwrap(), None).await.unwrap();
        assert_eq!(got.opened.meta.kind, Kind::Site);
        let f = &got.opened.meta.files["index.html"];
        assert_eq!(&got.opened.content[f.offset as usize..(f.offset + f.len) as usize], b"<h1>v2</h1>");
        let out = d.path().join("out");
        assert_eq!(unpack_site(&got.opened, &out).unwrap(), 2);
        assert_eq!(std::fs::read(out.join("css/a.css")).unwrap(), b"h1{color:red}");
        // Rekey: the old link stops working.
        put_site(&s, &alice, &site_dir, "family-photos", true).await.unwrap();
        assert!(get(&s, &ObjectRef::parse(&link).unwrap(), None).await.is_err());
    }

    #[tokio::test]
    async fn multipart_objects_reassemble_and_revoke_fully() {
        let (_d, s) = store();
        let alice = StorageSecret::generate();
        let bob = StorageSecret::generate();
        let data: Vec<u8> = (0..350_000u32).map(|i| (i % 253) as u8).collect();
        let put = put_bytes_split(&s, &alice, &data, PutOptions { share_with: vec![bob.public()], ..Default::default() }, 100_000).await.unwrap();
        assert_eq!(put.published.len(), 5, "4 parts + root");
        let r = ObjectRef { site_id: put.site_id.clone(), key: None, path: None };
        let got = get(&s, &r, Some(&bob)).await.unwrap();
        assert_eq!(got.opened.meta.kind, Kind::Multipart);
        assert_eq!(&*got.opened.content, &data);
        let old_parts = got.opened.meta.parts.clone();
        revoke(&s, &alice, &put.site_id, &[bob.public()]).await.unwrap();
        assert!(get(&s, &r, Some(&bob)).await.is_err());
        assert_eq!(&*get(&s, &r, Some(&alice)).await.unwrap().opened.content, &data);
        // Bob kept the old part keys: the old part objects are tombstones now.
        let pkey = DataKey::from_link_secret(&old_parts[0].key).unwrap();
        let pblob = s.get(&old_parts[0].site_id).await.unwrap();
        assert!(c0mpute_sealed::open(&pblob, &pkey, None).is_err());
    }

    #[tokio::test]
    async fn volumes_persist_and_hide_names() {
        let (d, s) = store();
        let s: std::sync::Arc<dyn BlobStore> = std::sync::Arc::new(s);
        let alice = std::sync::Arc::new(StorageSecret::generate());
        let mut v = Volume::open(s.clone(), alice.clone(), "backups").await.unwrap();
        v.mkdir("docs").await.unwrap();
        v.write("docs/passwords-2026.txt", b"hunter2 hunter2").await.unwrap();
        v.write("photo.jpg", &[9u8; 70_000]).await.unwrap();
        v.write("docs/passwords-2026.txt", b"rotated: correct horse").await.unwrap();
        v.rename("photo.jpg", "docs/photo.jpg").await.unwrap();
        let kids: Vec<_> = v.children("").collect();
        assert_eq!(kids, vec![("docs".to_string(), true)]);
        // Reopen from the store alone: everything is there.
        let v2 = Volume::open(s.clone(), alice.clone(), "backups").await.unwrap();
        assert_eq!(&*v2.read("docs/passwords-2026.txt").await.unwrap(), b"rotated: correct horse");
        assert_eq!(v2.read("docs/photo.jpg").await.unwrap().len(), 70_000);
        // Another identity cannot open the volume; a fresh label is empty.
        assert!(Volume::open(s.clone(), std::sync::Arc::new(StorageSecret::generate()), "backups").await.unwrap().index.files.is_empty());
        assert!(Volume::open(s.clone(), alice.clone(), "other").await.unwrap().index.files.is_empty());
        // Nothing readable in the stored blobs.
        for e in std::fs::read_dir(d.path().join("blobs")).unwrap() {
            let b = std::fs::read(e.unwrap().path()).unwrap();
            for needle in [&b"hunter2"[..], b"passwords-2026", b"correct horse", b"docs/"] {
                assert!(!b.windows(needle.len()).any(|w| w == needle));
            }
        }
    }

    #[test]
    fn local_index_round_trips_and_is_encrypted() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("idx");
        let alice = StorageSecret::generate();
        let mut idx = LocalIndex::default();
        idx.objects.insert(
            "0123456789abcdef0123".into(),
            IndexEntry { file_name: Some("tax-return-2026.pdf".into()), size: 5, kind: Kind::File, created: 1, shared_with: 0 },
        );
        idx.save(&p, &alice).unwrap();
        let raw = std::fs::read(&p).unwrap();
        assert!(!raw.windows(10).any(|w| w == b"tax-return"));
        assert_eq!(LocalIndex::load(&p, &alice).unwrap().objects.len(), 1);
        assert!(LocalIndex::load(&p, &StorageSecret::generate()).is_err());
    }
}
