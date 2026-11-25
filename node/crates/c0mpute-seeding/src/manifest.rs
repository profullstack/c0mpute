//! The seeding manifest: the signed list of torrents c0mpute publishes.
//!
//! A node seeds a torrent from the network list only when it is named in a
//! manifest whose minisign signature verifies against the key pinned in the
//! binary ([`PINNED_PUBKEY`], the release key). The signature's trusted
//! comment must name [`MANIFEST_FILE`], so a signature made for a release
//! artifact cannot be passed off as a manifest (and the other way round).
//!
//! Anything doubtful refuses the whole manifest: a bad signature, an unknown
//! kind or version, an expired manifest, a sequence number older than the one
//! already seen, or a single malformed entry.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

/// The key every manifest is checked against: the c0mpute release key
/// (`minisign.pub` at the repo root). Not configurable.
pub const PINNED_PUBKEY: &str = c0mpute_update::RELEASE_PUBKEY;

/// `kind` of a seeding manifest.
pub const MANIFEST_KIND: &str = "c0mpute.seeding.manifest";
/// The manifest format this build reads.
pub const MANIFEST_VERSION: u32 = 1;
/// The file name the signature's trusted comment must carry
/// (`file:seeding-manifest.json`), whatever the manifest is called on disk.
pub const MANIFEST_FILE: &str = "seeding-manifest.json";
/// Where nodes fetch the manifest by default; the signature is `<url>.minisig`.
pub const DEFAULT_MANIFEST_URL: &str = "https://c0mpute.com/seeding/manifest.json";

/// Largest manifest a node reads.
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
/// Largest signature file a node reads.
pub const MAX_SIGNATURE_BYTES: usize = 4096;
/// Most torrents in one manifest.
pub const MAX_TORRENTS: usize = 1024;
/// Most trackers on one torrent.
pub const MAX_TRACKERS: usize = 16;

/// One torrent c0mpute publishes and asks nodes to seed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// BitTorrent v1 info hash, 40 hex characters.
    pub info_hash: String,
    /// What it is, for people.
    pub name: String,
    /// Total size of the torrent's files in bytes. The node plans its disk
    /// cap from this and refuses the torrent if it turns out larger.
    pub bytes: u64,
    /// The license that makes it legal for a node to redistribute this
    /// (an SPDX id or a short statement). Never empty.
    pub license: String,
    /// The https page where we publish it.
    pub source: String,
    /// Trackers the node announces to. There is no DHT, so a torrent without
    /// a tracker is found only by peers that already know this node.
    #[serde(default)]
    pub trackers: Vec<String>,
}

/// The signed list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub kind: String,
    pub v: u32,
    /// Goes up with every published manifest. A node refuses one older than
    /// the newest it has accepted, so a withdrawn torrent cannot be brought
    /// back by replaying an old signed file.
    pub seq: u64,
    /// Unix seconds.
    pub issued_at: u64,
    /// Unix seconds after which nodes stop seeding from this manifest;
    /// `0` or absent: no expiry.
    #[serde(default)]
    pub expires_at: u64,
    pub torrents: Vec<Entry>,
}

/// The list compiled into the binary. Empty: every torrent a node seeds from
/// us comes from a signed manifest.
pub const BUILTIN: &[BuiltinEntry] = &[];

/// A compiled-in [`Entry`].
#[derive(Clone, Copy, Debug)]
pub struct BuiltinEntry {
    pub info_hash: &'static str,
    pub name: &'static str,
    pub bytes: u64,
    pub license: &'static str,
    pub source: &'static str,
    pub trackers: &'static [&'static str],
}

/// [`BUILTIN`] as entries.
pub fn builtin_entries() -> Vec<Entry> {
    BUILTIN
        .iter()
        .map(|b| Entry {
            info_hash: b.info_hash.to_string(),
            name: b.name.to_string(),
            bytes: b.bytes,
            license: b.license.to_string(),
            source: b.source.to_string(),
            trackers: b.trackers.iter().map(|t| t.to_string()).collect(),
        })
        .collect()
}

/// `raw` as a lowercase 40-hex v1 info hash, or `None`.
pub fn normalize_info_hash(raw: &str) -> Option<String> {
    let h = raw.trim().to_ascii_lowercase();
    (h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

fn printable(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}

/// Check one entry the way a node does (and lower-case its hash). Used by
/// `c0mpute seeding entry` so the publisher sees a refusal before signing.
pub fn check_entry(e: &mut Entry) -> Result<()> {
    e.info_hash = normalize_info_hash(&e.info_hash)
        .with_context(|| format!("'{}' is not a 40-hex info hash", e.info_hash))?;
    ensure!(printable(&e.name, 200), "{}: bad name", e.info_hash);
    ensure!(e.bytes > 0, "{}: bytes must be greater than 0", e.info_hash);
    ensure!(
        printable(&e.license, 200),
        "{}: a license is required",
        e.info_hash
    );
    let source = url::Url::parse(&e.source)
        .with_context(|| format!("{}: source is not a URL", e.info_hash))?;
    ensure!(
        source.scheme() == "https",
        "{}: source must be an https URL",
        e.info_hash
    );
    ensure!(
        e.trackers.len() <= MAX_TRACKERS,
        "{}: more than {MAX_TRACKERS} trackers",
        e.info_hash
    );
    for t in &e.trackers {
        let u = url::Url::parse(t)
            .with_context(|| format!("{}: tracker '{t}' is not a URL", e.info_hash))?;
        ensure!(
            matches!(u.scheme(), "udp" | "http" | "https") && t.len() <= 512,
            "{}: tracker '{t}' must be a udp, http or https URL",
            e.info_hash
        );
    }
    Ok(())
}

fn verify_inner(
    pubkey_b64: &str,
    bytes: &[u8],
    sig_text: &str,
    now_secs: Option<u64>,
) -> Result<Manifest> {
    ensure!(
        bytes.len() <= MAX_MANIFEST_BYTES,
        "manifest is larger than {MAX_MANIFEST_BYTES} bytes"
    );
    ensure!(
        sig_text.len() <= MAX_SIGNATURE_BYTES,
        "manifest signature is too large"
    );
    // Signature first: nothing in an unsigned file is read.
    c0mpute_update::verify_release_signature(pubkey_b64, bytes, sig_text, MANIFEST_FILE, None)?;
    let mut m: Manifest =
        serde_json::from_slice(bytes).context("signed manifest is not valid manifest JSON")?;
    ensure!(
        m.kind == MANIFEST_KIND,
        "signed file is not a seeding manifest (kind '{}')",
        m.kind
    );
    ensure!(
        m.v == MANIFEST_VERSION,
        "manifest version {} is not supported by this build",
        m.v
    );
    if let Some(now) = now_secs
        && m.expires_at != 0
        && now > m.expires_at
    {
        bail!("manifest {} expired at {}", m.seq, m.expires_at);
    }
    ensure!(
        m.torrents.len() <= MAX_TORRENTS,
        "manifest lists more than {MAX_TORRENTS} torrents"
    );
    let mut seen = HashSet::new();
    for e in &mut m.torrents {
        check_entry(e)?;
        ensure!(
            seen.insert(e.info_hash.clone()),
            "{} is listed twice",
            e.info_hash
        );
    }
    Ok(m)
}

/// Verify `bytes` (the manifest file) against `sig_text` (its `.minisig`)
/// with `pubkey_b64`, and check everything in it. `now_secs` is unix time.
pub fn verify(pubkey_b64: &str, bytes: &[u8], sig_text: &str, now_secs: u64) -> Result<Manifest> {
    verify_inner(pubkey_b64, bytes, sig_text, Some(now_secs))
}

/// The last manifest a node accepted, kept on disk so it keeps seeding while
/// the manifest URL is unreachable. The cached files are verified again every
/// time they are read: nothing is trusted because it is on disk.
pub struct ManifestStore {
    dir: PathBuf,
    pubkey: String,
}

impl ManifestStore {
    /// A store under `dir`, checking against the pinned key.
    pub fn new(dir: &Path) -> Self {
        Self::with_pubkey(dir, PINNED_PUBKEY)
    }

    /// A store checking against `pubkey_b64` (tests use a throwaway key).
    pub fn with_pubkey(dir: &Path, pubkey_b64: &str) -> Self {
        Self {
            dir: dir.to_path_buf(),
            pubkey: pubkey_b64.to_string(),
        }
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.join("manifest.json")
    }

    fn sig_path(&self) -> PathBuf {
        self.dir.join("manifest.json.minisig")
    }

    fn read_cached(&self) -> Option<(Vec<u8>, String)> {
        let bytes = std::fs::read(self.manifest_path()).ok()?;
        let sig = std::fs::read_to_string(self.sig_path()).ok()?;
        Some((bytes, sig))
    }

    /// The highest sequence number accepted so far (from a cached manifest
    /// that still verifies, expired or not).
    pub fn last_seq(&self) -> Option<u64> {
        let (bytes, sig) = self.read_cached()?;
        verify_inner(&self.pubkey, &bytes, &sig, None)
            .ok()
            .map(|m| m.seq)
    }

    /// The cached manifest, when it verifies and has not expired.
    pub fn cached(&self, now_secs: u64) -> Option<Manifest> {
        let (bytes, sig) = self.read_cached()?;
        verify(&self.pubkey, &bytes, &sig, now_secs).ok()
    }

    /// Verify a freshly fetched manifest and keep it. Refused when it does
    /// not verify or is older than the one already accepted.
    pub fn accept(&self, bytes: &[u8], sig_text: &str, now_secs: u64) -> Result<Manifest> {
        let m = verify(&self.pubkey, bytes, sig_text, now_secs)?;
        if let Some(last) = self.last_seq() {
            ensure!(
                m.seq >= last,
                "manifest {} is older than {last}, the newest this node has accepted",
                m.seq
            );
        }
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("create {}", self.dir.display()))?;
        // Signature last: a manifest without its signature is never read.
        let _ = std::fs::remove_file(self.sig_path());
        write_replace(&self.manifest_path(), bytes)?;
        write_replace(&self.sig_path(), sig_text.as_bytes())?;
        Ok(m)
    }
}

fn write_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

async fn fetch_capped(http: &reqwest::Client, url: &str, max: usize) -> Result<Vec<u8>> {
    let mut res = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetch {url}"))?;
    ensure!(res.status().is_success(), "{url} answered {}", res.status());
    let mut out = Vec::new();
    while let Some(chunk) = res.chunk().await.with_context(|| format!("read {url}"))? {
        ensure!(
            out.len() + chunk.len() <= max,
            "{url} is larger than {max} bytes"
        );
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Download the manifest at `url` and its signature at `<url>.minisig`.
/// Nothing is verified here; hand both to [`ManifestStore::accept`].
pub async fn fetch(url: &str) -> Result<(Vec<u8>, String)> {
    let parsed = url::Url::parse(url).with_context(|| format!("manifest URL '{url}'"))?;
    ensure!(
        matches!(parsed.scheme(), "https" | "http"),
        "manifest URL must be http or https"
    );
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let bytes = fetch_capped(&http, url, MAX_MANIFEST_BYTES).await?;
    let sig = fetch_capped(&http, &format!("{url}.minisig"), MAX_SIGNATURE_BYTES).await?;
    let sig = String::from_utf8(sig).context("manifest signature is not text")?;
    Ok((bytes, sig))
}

#[cfg(test)]
pub(crate) mod testkit {
    use super::*;

    /// A throwaway minisign key for tests.
    pub struct TestKey {
        pub pk_b64: String,
        kp: minisign::KeyPair,
    }

    impl TestKey {
        pub fn new() -> Self {
            let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
            Self {
                pk_b64: kp.pk.to_base64(),
                kp,
            }
        }

        /// A `.minisig` over `bytes` whose trusted comment names `file`.
        pub fn sign_as(&self, bytes: &[u8], file: &str) -> String {
            let comment = format!("timestamp:1 file:{file}");
            minisign::sign(
                Some(&self.kp.pk),
                &self.kp.sk,
                std::io::Cursor::new(bytes),
                Some(&comment),
                Some("signature from a test key"),
            )
            .unwrap()
            .into_string()
        }

        pub fn sign(&self, bytes: &[u8]) -> String {
            self.sign_as(bytes, MANIFEST_FILE)
        }
    }

    pub const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    pub const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    pub fn entry(info_hash: &str, bytes: u64) -> Entry {
        Entry {
            info_hash: info_hash.to_string(),
            name: format!("test torrent {}", &info_hash[..4]),
            bytes,
            license: "CC0-1.0".to_string(),
            source: "https://example.org/releases".to_string(),
            trackers: vec!["udp://tracker.example.org:1337/announce".to_string()],
        }
    }

    pub fn manifest(seq: u64, torrents: Vec<Entry>) -> Manifest {
        Manifest {
            kind: MANIFEST_KIND.to_string(),
            v: MANIFEST_VERSION,
            seq,
            issued_at: 1_800_000_000,
            expires_at: 0,
            torrents,
        }
    }

    pub fn bytes_of(m: &Manifest) -> Vec<u8> {
        serde_json::to_vec_pretty(m).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;

    const NOW: u64 = 1_800_000_100;

    #[test]
    fn the_builtin_list_is_empty() {
        assert!(BUILTIN.is_empty());
        assert!(builtin_entries().is_empty());
    }

    #[test]
    fn a_manifest_signed_by_the_pinned_key_is_read() {
        let key = TestKey::new();
        let m = manifest(1, vec![entry(HASH_A, 10), entry(HASH_B, 20)]);
        let bytes = bytes_of(&m);
        let got = verify(&key.pk_b64, &bytes, &key.sign(&bytes), NOW).unwrap();
        assert_eq!(got, m);
    }

    #[test]
    fn a_wrong_signature_is_refused() {
        let key = TestKey::new();
        let other = TestKey::new();
        let bytes = bytes_of(&manifest(1, vec![entry(HASH_A, 10)]));
        let sig = key.sign(&bytes);

        // Signed by a key that is not the pinned one.
        assert!(verify(&other.pk_b64, &bytes, &sig, NOW).is_err());
        // The real pinned key did not sign a test manifest either.
        assert!(verify(PINNED_PUBKEY, &bytes, &sig, NOW).is_err());
        // One byte changed after signing (a torrent swapped in).
        let mut tampered = bytes_of(&manifest(1, vec![entry(HASH_B, 10)]));
        assert!(verify(&key.pk_b64, &tampered, &sig, NOW).is_err());
        tampered.push(b' ');
        assert!(verify(&key.pk_b64, &tampered, &sig, NOW).is_err());
        // No signature, or something that is not one.
        assert!(verify(&key.pk_b64, &bytes, "", NOW).is_err());
        assert!(verify(&key.pk_b64, &bytes, "<html>404</html>", NOW).is_err());
        // A valid signature by the right key, made for another file (a
        // release artifact, say), does not make a manifest.
        let as_release = key.sign_as(&bytes, "c0mpute-linux-x86_64.tar.gz");
        assert!(verify(&key.pk_b64, &bytes, &as_release, NOW).is_err());
    }

    #[test]
    fn a_signed_file_that_is_not_a_good_manifest_is_refused() {
        let key = TestKey::new();
        let check = |m: &Manifest| {
            let bytes = bytes_of(m);
            verify(&key.pk_b64, &bytes, &key.sign(&bytes), NOW)
        };
        let good = manifest(1, vec![entry(HASH_A, 10)]);
        assert!(check(&good).is_ok());

        let mut m = good.clone();
        m.kind = "c0mpute.release".into();
        assert!(check(&m).is_err());
        let mut m = good.clone();
        m.v = 2;
        assert!(check(&m).is_err());
        let mut m = good.clone();
        m.expires_at = NOW - 1;
        assert!(check(&m).is_err());
        m.expires_at = NOW + 1;
        assert!(check(&m).is_ok());

        let bad_entry = |f: fn(&mut Entry)| {
            let mut e = entry(HASH_A, 10);
            f(&mut e);
            check(&manifest(1, vec![e]))
        };
        assert!(bad_entry(|e| e.info_hash = "abc".into()).is_err());
        assert!(bad_entry(|e| e.info_hash = "z".repeat(40)).is_err());
        assert!(bad_entry(|e| e.bytes = 0).is_err());
        assert!(bad_entry(|e| e.license = " ".into()).is_err());
        assert!(bad_entry(|e| e.name = String::new()).is_err());
        assert!(bad_entry(|e| e.source = "http://example.org/".into()).is_err());
        assert!(bad_entry(|e| e.source = "not a url".into()).is_err());
        assert!(bad_entry(|e| e.trackers = vec!["wss://tracker.example.org".into()]).is_err());
        assert!(bad_entry(|e| e.trackers = vec!["magnet:?xt=urn:btih:x".into()]).is_err());
        // Listed twice (also when the case differs).
        let twice = manifest(
            1,
            vec![entry(HASH_A, 10), entry(&HASH_A.to_uppercase(), 10)],
        );
        assert!(check(&twice).is_err());
        // An upper-case hash alone is fine and comes back lower-case.
        let upper = check(&manifest(1, vec![entry(&HASH_A.to_uppercase(), 10)])).unwrap();
        assert_eq!(upper.torrents[0].info_hash, HASH_A);
        // Signed, but not JSON.
        assert!(verify(&key.pk_b64, b"hello", &key.sign(b"hello"), NOW).is_err());
    }

    #[test]
    fn the_store_refuses_replays_and_rechecks_its_cache() {
        let key = TestKey::new();
        let dir = tempfile::tempdir().unwrap();
        let store = ManifestStore::with_pubkey(dir.path(), &key.pk_b64);
        assert!(store.cached(NOW).is_none());
        assert_eq!(store.last_seq(), None);

        let v2 = bytes_of(&manifest(2, vec![entry(HASH_A, 10), entry(HASH_B, 10)]));
        store.accept(&v2, &key.sign(&v2), NOW).unwrap();
        assert_eq!(store.cached(NOW).unwrap().torrents.len(), 2);

        // HASH_B is withdrawn in 3.
        let v3 = bytes_of(&manifest(3, vec![entry(HASH_A, 10)]));
        store.accept(&v3, &key.sign(&v3), NOW).unwrap();
        assert_eq!(store.cached(NOW).unwrap().torrents.len(), 1);
        // Replaying the older, validly signed manifest does not bring it back.
        let err = store.accept(&v2, &key.sign(&v2), NOW).unwrap_err();
        assert!(err.to_string().contains("older"), "{err}");
        assert_eq!(store.cached(NOW).unwrap().seq, 3);
        // An unsigned replacement is refused and the cache is untouched.
        let v9 = bytes_of(&manifest(9, vec![entry(HASH_B, 10)]));
        assert!(store.accept(&v9, "", NOW).is_err());
        assert_eq!(store.cached(NOW).unwrap().seq, 3);

        // A cache file edited on disk no longer verifies: nothing is read.
        std::fs::write(dir.path().join("manifest.json"), &v9).unwrap();
        assert!(store.cached(NOW).is_none());
        assert_eq!(store.last_seq(), None);
        // A store with another key does not read this cache either.
        std::fs::write(dir.path().join("manifest.json"), &v3).unwrap();
        assert!(store.cached(NOW).is_some());
        let other = TestKey::new();
        assert!(
            ManifestStore::with_pubkey(dir.path(), &other.pk_b64)
                .cached(NOW)
                .is_none()
        );
    }

    #[test]
    fn an_expired_cache_stops_being_used_but_still_blocks_replays() {
        let key = TestKey::new();
        let dir = tempfile::tempdir().unwrap();
        let store = ManifestStore::with_pubkey(dir.path(), &key.pk_b64);
        let mut m = manifest(5, vec![entry(HASH_A, 10)]);
        m.expires_at = NOW + 10;
        let bytes = bytes_of(&m);
        store.accept(&bytes, &key.sign(&bytes), NOW).unwrap();
        assert!(store.cached(NOW).is_some());
        assert!(store.cached(NOW + 11).is_none());
        assert_eq!(store.last_seq(), Some(5));
        let old = bytes_of(&manifest(4, vec![entry(HASH_B, 10)]));
        assert!(store.accept(&old, &key.sign(&old), NOW + 11).is_err());
    }
}
