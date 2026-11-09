//! What a deployed app is: an owner-signed [`AppRecord`] naming a container
//! image (published as a hosting site, or a digest-pinned registry ref), the
//! port it listens on, its public environment, where its state lives, and how
//! many copies the network keeps.
//!
//! Like a site record, everything is verifiable from the bytes alone: the app
//! id is derived from the owner key and name, so only the owner can publish a
//! new version, and the highest `seq` wins.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};

use crate::hosting::site::{valid_domain, valid_name};

/// Copies the network keeps of every app (one primary, the rest standbys).
pub const DEFAULT_REPLICAS: u8 = 3;
pub const MIN_REPLICAS: u8 = 2;
pub const MAX_REPLICAS: u8 = 5;
/// Most environment variables (public, in the record) an app may set.
pub const MAX_ENV: usize = 64;
pub const MAX_ENV_VALUE: usize = 4096;
pub const MAX_DOMAINS: usize = 8;
/// Memory an app may ask for, MiB.
pub const MIN_MEMORY_MB: u32 = 64;
pub const MAX_MEMORY_MB: u32 = 8192;
/// Shortest and longest snapshot interval.
pub const MIN_SNAPSHOT_SECS: u32 = 10;
pub const MAX_SNAPSHOT_SECS: u32 = 3600;
pub const DEFAULT_SNAPSHOT_SECS: u32 = 30;

/// Where the image comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageSource {
    /// A `docker save` tarball (optionally gzipped) published as a hosting
    /// site by the owner: replicated by the network, no registry involved.
    Site {
        site_id: String,
        /// The site version (manifest hash) that holds this image.
        manifest: String,
        /// File inside the site, normally `image.tar.gz`.
        file: String,
        /// The `repo:tag` the tarball loads as.
        tag: String,
    },
    /// A registry image pinned by digest (`ghcr.io/x/y@sha256:...`).
    Pull { reference: String },
}

/// The owner's signed statement "app `name` is now this".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppRecord {
    pub v: u8,
    pub app_id: String,
    pub name: String,
    /// Owner's ed25519 public key, hex.
    pub owner: String,
    /// Version: higher wins. Publishers use milliseconds since the epoch.
    pub seq: u64,
    pub image: ImageSource,
    /// TCP port the container listens on for HTTP.
    pub port: u16,
    /// Path the primary probes; any answer below 500 is healthy.
    #[serde(default = "default_health")]
    pub health_path: String,
    /// Public environment. Secrets never ride here: see `secrets_hash`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// blake3 (hex) of the sealed-secrets blob the owner hands to holders.
    /// `None` when the app has no secrets and no state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets_hash: Option<String>,
    /// Absolute path inside the container that holds the app's state. The
    /// primary snapshots it; a standby restores it on failover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_path: Option<String>,
    #[serde(default = "default_snapshot_secs")]
    pub snapshot_secs: u32,
    #[serde(default = "default_memory")]
    pub memory_mb: u32,
    /// CPU limit in thousandths of a core.
    #[serde(default = "default_millicpus")]
    pub millicpus: u32,
    #[serde(default = "default_replicas")]
    pub replicas: u8,
    /// Custom domains (served once DNS `TXT _c0mpute-app.<domain>` says
    /// `app=<app_id>`).
    #[serde(default)]
    pub domains: Vec<String>,
    /// A tombstone: holders stop the app and drop its copies.
    #[serde(default)]
    pub deleted: bool,
    /// ed25519 signature (hex) over [`AppRecord::signing_bytes`].
    pub sig: String,
}

fn default_health() -> String {
    "/".into()
}
fn default_snapshot_secs() -> u32 {
    DEFAULT_SNAPSHOT_SECS
}
fn default_memory() -> u32 {
    256
}
fn default_millicpus() -> u32 {
    500
}
fn default_replicas() -> u8 {
    DEFAULT_REPLICAS
}

/// The id an app with this owner and name always has (20 hex characters).
pub fn app_id_for(owner_hex: &str, name: &str) -> String {
    let h = Hash::of(format!("c0mpute-app\n{owner_hex}\n{name}").as_bytes()).to_hex();
    h[..20].to_string()
}

/// `true` for an app id (20 lowercase hex characters).
pub fn valid_app_id(s: &str) -> bool {
    s.len() == 20
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_env_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 128
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !k.starts_with(|c: char| c.is_ascii_digit())
}

/// A registry ref is accepted only when pinned by a sha256 digest, so every
/// holder runs the same bytes.
pub fn valid_pinned_ref(r: &str) -> bool {
    let Some((name, digest)) = r.split_once("@sha256:") else {
        return false;
    };
    !name.is_empty()
        && name.len() <= 255
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "./-_:".contains(c))
        && digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_tag(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 200
        && t.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "./-_:".contains(c))
}

/// What a deployer fills in; [`AppRecord::sign`] turns it into a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppSpec {
    pub name: String,
    pub image: ImageSource,
    pub port: u16,
    pub health_path: String,
    pub env: BTreeMap<String, String>,
    pub secrets_hash: Option<String>,
    pub state_path: Option<String>,
    pub snapshot_secs: u32,
    pub memory_mb: u32,
    pub millicpus: u32,
    pub replicas: u8,
    pub domains: Vec<String>,
}

impl AppRecord {
    /// The bytes the owner signs: the record as JSON with an empty `sig`
    /// (struct order and `BTreeMap` keep it canonical).
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut c = self.clone();
        c.sig = String::new();
        let mut out = b"c0mpute-app-record\n".to_vec();
        out.extend(serde_json::to_vec(&c).unwrap_or_default());
        out
    }

    pub fn sign(
        keypair: &c0mpute_net::libp2p::identity::Keypair,
        spec: AppSpec,
        seq: u64,
    ) -> Result<Self> {
        let Some(owner) = crate::hosting::site::owner_hex(keypair) else {
            bail!("apps are signed with an ed25519 key");
        };
        let mut rec = AppRecord {
            v: 1,
            app_id: app_id_for(&owner, &spec.name),
            name: spec.name,
            owner,
            seq,
            image: spec.image,
            port: spec.port,
            health_path: spec.health_path,
            env: spec.env,
            secrets_hash: spec.secrets_hash,
            state_path: spec.state_path,
            snapshot_secs: spec.snapshot_secs,
            memory_mb: spec.memory_mb,
            millicpus: spec.millicpus,
            replicas: spec.replicas,
            domains: spec.domains,
            deleted: false,
            sig: String::new(),
        };
        rec.check_fields()?;
        rec.sig = hex::encode(keypair.sign(&rec.signing_bytes())?);
        Ok(rec)
    }

    /// A tombstone for `prev`: same app, higher seq, `deleted`.
    pub fn tombstone(
        keypair: &c0mpute_net::libp2p::identity::Keypair,
        prev: &AppRecord,
        seq: u64,
    ) -> Result<Self> {
        let mut rec = prev.clone();
        if crate::hosting::site::owner_hex(keypair).as_deref() != Some(prev.owner.as_str()) {
            bail!("only the owner can delete an app");
        }
        rec.seq = seq.max(prev.seq + 1);
        rec.deleted = true;
        rec.sig = String::new();
        rec.sig = hex::encode(keypair.sign(&rec.signing_bytes())?);
        Ok(rec)
    }

    pub fn check_fields(&self) -> Result<()> {
        if self.v != 1 {
            bail!("unsupported app record version {}", self.v);
        }
        if !valid_name(&self.name) {
            bail!("app name must be 1-63 of a-z, 0-9 and '-'");
        }
        if self.app_id != app_id_for(&self.owner, &self.name) {
            bail!("app id does not match owner and name");
        }
        match &self.image {
            ImageSource::Site {
                site_id,
                manifest,
                file,
                tag,
            } => {
                if !valid_app_id(site_id) || Hash::from_hex(manifest).is_err() {
                    bail!("image site id or manifest is malformed");
                }
                if !crate::hosting::site::valid_path(file) || !valid_tag(tag) {
                    bail!("image file or tag is malformed");
                }
            }
            ImageSource::Pull { reference } => {
                if !valid_pinned_ref(reference) {
                    bail!("a registry image must be pinned: name@sha256:<64 hex>");
                }
            }
        }
        if self.port == 0 {
            bail!("port must be 1-65535");
        }
        if !self.health_path.starts_with('/')
            || self.health_path.len() > 256
            || self.health_path.contains(['\r', '\n', ' '])
        {
            bail!("health path must be an absolute URL path");
        }
        if self.env.len() > MAX_ENV {
            bail!("at most {MAX_ENV} environment variables");
        }
        for (k, v) in &self.env {
            if !valid_env_key(k) || v.len() > MAX_ENV_VALUE || v.contains('\0') {
                bail!("bad environment variable {k:?}");
            }
        }
        if let Some(h) = &self.secrets_hash
            && Hash::from_hex(h).is_err()
        {
            bail!("secrets hash is not a hash");
        }
        if let Some(p) = &self.state_path
            && (!p.starts_with('/')
                || p == "/"
                || p.len() > 256
                || p.contains("..")
                || p.contains(['\0', '\n', ':']))
        {
            bail!("state path must be an absolute directory other than /");
        }
        if self.state_path.is_some() && self.secrets_hash.is_none() {
            // Snapshots are encrypted with the app key, which rides in the
            // sealed secrets: no secrets, no key, no state.
            bail!("an app with state needs sealed secrets (they carry its snapshot key)");
        }
        if !(MIN_SNAPSHOT_SECS..=MAX_SNAPSHOT_SECS).contains(&self.snapshot_secs) {
            bail!("snapshot interval must be {MIN_SNAPSHOT_SECS}-{MAX_SNAPSHOT_SECS} s");
        }
        if !(MIN_MEMORY_MB..=MAX_MEMORY_MB).contains(&self.memory_mb) {
            bail!("memory must be {MIN_MEMORY_MB}-{MAX_MEMORY_MB} MiB");
        }
        if !(100..=8000).contains(&self.millicpus) {
            bail!("cpu must be 0.1-8 cores");
        }
        if !(MIN_REPLICAS..=MAX_REPLICAS).contains(&self.replicas) {
            bail!("replicas must be {MIN_REPLICAS}-{MAX_REPLICAS}");
        }
        if self.domains.len() > MAX_DOMAINS || !self.domains.iter().all(|d| valid_domain(d)) {
            bail!("invalid domain list");
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
        let Ok(public) = c0mpute_net::libp2p::identity::ed25519::PublicKey::try_from_bytes(&key)
        else {
            return false;
        };
        public.verify(&self.signing_bytes(), &sig)
    }

    /// The owner's libp2p peer id (base58), for owner-only RPCs.
    pub fn owner_peer(&self) -> Option<String> {
        let key = hex::decode(&self.owner).ok()?;
        let public =
            c0mpute_net::libp2p::identity::ed25519::PublicKey::try_from_bytes(&key).ok()?;
        let pk: c0mpute_net::libp2p::identity::PublicKey = public.into();
        Some(pk.to_peer_id().to_base58())
    }

    /// The docker image reference a holder runs.
    pub fn image_ref(&self) -> &str {
        match &self.image {
            ImageSource::Site { tag, .. } => tag,
            ImageSource::Pull { reference } => reference,
        }
    }

    /// The app needs sealed material from the owner (secrets, or the key its
    /// state snapshots are encrypted with) before it can run.
    pub fn needs_secrets(&self) -> bool {
        self.secrets_hash.is_some()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn spec(name: &str) -> AppSpec {
        AppSpec {
            name: name.into(),
            image: ImageSource::Pull {
                reference: format!("ghcr.io/profullstack/demo@sha256:{}", "a".repeat(64)),
            },
            port: 8080,
            health_path: "/healthz".into(),
            env: BTreeMap::from([("NODE_ENV".into(), "production".into())]),
            secrets_hash: Some(Hash::of(b"s").to_hex()),
            state_path: Some("/data".into()),
            snapshot_secs: 30,
            memory_mb: 256,
            millicpus: 500,
            replicas: 3,
            domains: vec!["api.example.org".into()],
        }
    }

    #[test]
    fn sign_verify_and_tamper() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let rec = AppRecord::sign(&kp, spec("demo"), 1).unwrap();
        assert!(rec.verify());
        assert_eq!(rec.app_id, app_id_for(&rec.owner, "demo"));
        assert_eq!(
            rec.owner_peer().unwrap(),
            kp.public().to_peer_id().to_base58()
        );
        let mut bad = rec.clone();
        bad.port = 9090;
        assert!(!bad.verify(), "a changed field breaks the signature");
        let mut bad = rec.clone();
        bad.env.insert("X".into(), "y".into());
        assert!(!bad.verify());
        let json = serde_json::to_string(&rec).unwrap();
        let back: AppRecord = serde_json::from_str(&json).unwrap();
        assert!(back.verify(), "the record survives a JSON round trip");
    }

    #[test]
    fn tombstone_only_by_owner() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let other = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let rec = AppRecord::sign(&kp, spec("demo"), 5).unwrap();
        let t = AppRecord::tombstone(&kp, &rec, 1).unwrap();
        assert!(t.verify() && t.deleted && t.seq == 6);
        assert!(AppRecord::tombstone(&other, &rec, 9).is_err());
    }

    #[test]
    fn refuses_bad_specs() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let mut s = spec("demo");
        s.image = ImageSource::Pull {
            reference: "nginx:latest".into(),
        };
        assert!(
            AppRecord::sign(&kp, s, 1).is_err(),
            "unpinned images are refused"
        );
        let mut s = spec("demo");
        s.state_path = Some("/".into());
        assert!(AppRecord::sign(&kp, s, 1).is_err());
        let mut s = spec("demo");
        s.replicas = 1;
        assert!(
            AppRecord::sign(&kp, s, 1).is_err(),
            "one copy is not redundancy"
        );
        let mut s = spec("demo");
        s.env.insert("BAD-KEY".into(), "x".into());
        assert!(AppRecord::sign(&kp, s, 1).is_err());
        let mut s = spec("Demo");
        s.name = "Demo".into();
        assert!(AppRecord::sign(&kp, s, 1).is_err());
    }
}
