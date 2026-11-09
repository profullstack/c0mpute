//! The owner's side: build an image, publish it, sign the app record, hand
//! the secrets to the holders, and wait until a primary answers.
//!
//! The image never touches a registry: `docker save` → gzip → a hosting site
//! signed by the owner, so the network keeps three copies of it and any new
//! holder can load it while the owner is offline. A registry image pinned by
//! digest is the other option (`--pull`).
//!
//! Secrets live on the owner's machine in `<data_dir>/apps/owner/<name>.json`
//! (mode 0600) together with the app key that encrypts the state snapshots,
//! so every redeploy keeps the same key and the network can still read old
//! snapshots.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::record::{AppRecord, AppSpec, ImageSource};
use super::snapshot::AppSecrets;
use super::{Apps, Options};
use crate::hosting::{self, Hosting, site};

/// Where the image comes from.
#[derive(Clone, Debug)]
pub enum Source {
    /// `docker build` this directory (it needs a Dockerfile).
    Build(PathBuf),
    /// An image already in the local docker.
    Local(String),
    /// A registry image pinned by digest; holders pull it themselves.
    Pull(String),
}

#[derive(Clone, Debug)]
pub struct DeployOptions {
    pub name: String,
    pub source: Source,
    pub port: u16,
    pub health_path: String,
    pub env: BTreeMap<String, String>,
    pub state_path: Option<String>,
    pub snapshot_secs: u32,
    pub memory_mb: u32,
    pub millicpus: u32,
    pub replicas: u8,
    pub domains: Vec<String>,
    pub wait: Duration,
}

/// What the owner keeps per app.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OwnerState {
    #[serde(default)]
    pub secrets: Option<AppSecrets>,
    #[serde(default)]
    pub record: Option<AppRecord>,
}

pub fn owner_dir() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("apps")
        .join("owner")
}

fn owner_path(name: &str) -> PathBuf {
    owner_dir().join(format!("{name}.json"))
}

pub fn load_owner(name: &str) -> OwnerState {
    std::fs::read(owner_path(name))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_owner(name: &str, st: &OwnerState) -> Result<()> {
    std::fs::create_dir_all(owner_dir())?;
    super::write_private(&owner_path(name), &serde_json::to_vec_pretty(st)?)?;
    Ok(())
}

/// Set (Some) or remove (None) a secret; takes effect on the next deploy.
pub fn set_secret(name: &str, key: &str, value: Option<&str>) -> Result<()> {
    if !site::valid_name(name) {
        bail!("app name must be 1-63 of a-z, 0-9 and '-'");
    }
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("secret names are letters, digits and '_'");
    }
    let mut st = load_owner(name);
    let s = st
        .secrets
        .get_or_insert_with(|| AppSecrets::generate(BTreeMap::new()));
    match value {
        Some(v) => s.env.insert(key.to_string(), v.to_string()),
        None => s.env.remove(key),
    };
    save_owner(name, &st)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn docker_status(args: &[&str]) -> Result<()> {
    let st = crate::container::command()
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
        .context("run docker (is it installed?)")?;
    if !st.success() {
        bail!("docker {} failed", args.first().unwrap_or(&""));
    }
    Ok(())
}

/// `docker save` an image, gzipped, into `dir/image.tar.gz`.
async fn save_image(tag: &str, dir: &Path) -> Result<u64> {
    let out = crate::container::command()
        .args(["save", tag])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .output()
        .await
        .context("docker save")?;
    if !out.status.success() {
        bail!("docker save {tag} failed");
    }
    let path = dir.join("image.tar.gz");
    let f = std::fs::File::create(&path)?;
    let mut gz = flate2::write::GzEncoder::new(f, flate2::Compression::default());
    gz.write_all(&out.stdout)?;
    gz.finish()?;
    Ok(std::fs::metadata(&path)?.len())
}

/// Join the network as the owner (no app hosting), with hosting and apps.
pub async fn owner_node() -> Result<(crate::Supervisor, Hosting, Apps)> {
    let supervisor = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    // Capability ads: how the owner finds apps nodes to dial.
    supervisor.registry.run(supervisor.libp2p.clone());
    let h = Hosting::start(
        supervisor.libp2p.clone(),
        supervisor.store.clone(),
        hosting::Options::default(),
    )
    .await?;
    let a = Apps::start(
        supervisor.libp2p.clone(),
        supervisor.store.clone(),
        Options {
            hosting: Some(h.clone()),
            registry: Some(supervisor.registry.clone()),
            ..Default::default()
        },
    )
    .await?;
    // Let the swarm reach peers before the first gossip.
    tokio::time::sleep(Duration::from_secs(5)).await;
    Ok((supervisor, h, a))
}

/// Deploy (or redeploy) an app and wait until it answers.
pub async fn deploy(opts: DeployOptions) -> Result<AppRecord> {
    if !site::valid_name(&opts.name) || opts.name.len() > 59 {
        bail!("app name must be 1-59 of a-z, 0-9 and '-'");
    }
    let keypair = hosting::publisher_keypair()?;
    let seq = now_ms();
    let mut st = load_owner(&opts.name);
    // The app key outlives every deploy; secrets are whatever is set now.
    if opts.state_path.is_some() && st.secrets.is_none() {
        st.secrets = Some(AppSecrets::generate(BTreeMap::new()));
    }
    let secrets = st
        .secrets
        .clone()
        .filter(|s| opts.state_path.is_some() || !s.env.is_empty());

    let (_sup, h, apps) = owner_node().await?;
    let mut image_site: Option<site::SiteRecord> = None;
    let image = match &opts.source {
        Source::Pull(reference) => ImageSource::Pull {
            reference: reference.clone(),
        },
        Source::Build(_) | Source::Local(_) => {
            let tag = format!("c0mpute-app/{}:{}", opts.name, seq);
            match &opts.source {
                Source::Build(dir) => {
                    eprintln!("building {} ...", dir.display());
                    docker_status(&["build", "--tag", &tag, &dir.display().to_string()]).await?;
                }
                Source::Local(img) => docker_status(&["tag", img, &tag]).await?,
                Source::Pull(_) => unreachable!(),
            }
            let tmp = std::env::temp_dir().join(format!("c0mpute-deploy-{}-{seq}", opts.name));
            std::fs::create_dir_all(&tmp)?;
            let size = save_image(&tag, &tmp).await?;
            if size > site::MAX_SITE_BYTES {
                let _ = std::fs::remove_dir_all(&tmp);
                bail!(
                    "the image is {} MiB gzipped; the limit is {} MiB (use a slim base image, or --pull a pinned registry image)",
                    size >> 20,
                    site::MAX_SITE_BYTES >> 20
                );
            }
            let site_name = format!("app-{}", opts.name);
            let built = site::build_from_dir(&tmp, &site_name)?;
            let _ = std::fs::remove_dir_all(&tmp);
            let manifest = built.manifest_hash();
            let rec = site::SiteRecord::sign(
                &keypair,
                &site_name,
                &manifest,
                seq,
                built.manifest.total_bytes(),
                vec![],
            )?;
            h.publish_site(&built, rec.clone()).await?;
            eprintln!(
                "image {tag} ({} MiB gzipped) published as site {}",
                size >> 20,
                rec.site_id
            );
            image_site = Some(rec.clone());
            ImageSource::Site {
                site_id: rec.site_id,
                manifest: manifest.to_hex(),
                file: "image.tar.gz".into(),
                tag,
            }
        }
    };

    let spec = AppSpec {
        name: opts.name.clone(),
        image,
        port: opts.port,
        health_path: opts.health_path.clone(),
        env: opts.env.clone(),
        secrets_hash: secrets.as_ref().map(|s| s.hash()),
        state_path: opts.state_path.clone(),
        snapshot_secs: opts.snapshot_secs,
        memory_mb: opts.memory_mb,
        millicpus: opts.millicpus,
        replicas: opts.replicas,
        domains: opts.domains.clone(),
    };
    let rec = AppRecord::sign(&keypair, spec, seq)?;
    if let Some(s) = &secrets {
        apps.own_secrets(&rec.app_id, s.clone()).await;
    }
    st.record = Some(rec.clone());
    save_owner(&opts.name, &st)?;
    apps.publish_record(rec.clone()).await?;
    eprintln!("app {} is {} (seq {})", rec.name, rec.app_id, rec.seq);

    wait_until_up(&apps, Some(&h), image_site.as_ref(), &rec, opts.wait).await;
    Ok(rec)
}

/// Keep gossiping (records, secrets, the image) until a primary is healthy
/// on the current version with every copy in place, or `wait` passes.
pub async fn wait_until_up(
    apps: &Apps,
    h: Option<&Hosting>,
    site: Option<&site::SiteRecord>,
    rec: &AppRecord,
    wait: Duration,
) {
    let started = Instant::now();
    let mut last = String::new();
    let mut last_gossip = Instant::now();
    while started.elapsed() < wait {
        tokio::time::sleep(Duration::from_secs(3)).await;
        apps.tick().await;
        if last_gossip.elapsed() > Duration::from_secs(15) {
            last_gossip = Instant::now();
            apps.publish_record(rec.clone()).await.ok();
            if let (Some(h), Some(s)) = (h, site) {
                h.regossip(s).await;
            }
        }
        let view = apps.live_view().await;
        let info = apps.info(rec, &view).await;
        let current = info
            .primary
            .as_ref()
            .and_then(|p| view.get(p))
            .and_then(|n| n.apps.get(&rec.app_id))
            .map(|a| a.seq == rec.seq)
            .unwrap_or(false);
        let image_copies = match (h, site) {
            (Some(h), Some(s)) => format!(", image {}/3 copies", h.holders(&s.site_id).await.len()),
            _ => String::new(),
        };
        let line = format!(
            "  holders {}/{}, primary {}{}{}",
            info.holders.len(),
            rec.replicas,
            info.primary.as_deref().map(short).unwrap_or("none yet"),
            if info.healthy && current {
                " (healthy)"
            } else {
                ""
            },
            image_copies
        );
        if line != last {
            eprintln!("{line}");
            last = line;
        }
        let done = info.healthy && current && info.holders.len() >= rec.replicas as usize;
        if done || (info.healthy && current && started.elapsed() > wait / 2) {
            eprintln!("up: /a/{}/ on any gateway", rec.app_id);
            return;
        }
    }
    eprintln!(
        "still converging after {} s; the network keeps going without this machine",
        wait.as_secs()
    );
}

fn short(p: &str) -> &str {
    &p[p.len().saturating_sub(8)..]
}

/// Delete an app: a signed tombstone, gossiped for a while.
pub async fn destroy(name: &str) -> Result<String> {
    let st = load_owner(name);
    let prev = st
        .record
        .context("no deployed app by that name on this machine")?;
    let keypair = hosting::publisher_keypair()?;
    let tomb = AppRecord::tombstone(&keypair, &prev, now_ms())?;
    let (_sup, _h, apps) = owner_node().await?;
    for _ in 0..6 {
        apps.publish_record(tomb.clone()).await?;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    let _ = std::fs::remove_file(owner_path(name));
    Ok(tomb.app_id)
}

/// Owner: read the primary's logs.
pub async fn logs(name_or_id: &str, tail: u32) -> Result<String> {
    let id = match load_owner(name_or_id).record {
        Some(r) => r.app_id,
        None => name_or_id.to_string(),
    };
    let (_sup, _h, apps) = owner_node().await?;
    // Hear at least one round of node announcements first.
    let started = Instant::now();
    loop {
        match apps.logs(&id, tail).await {
            Ok(text) => return Ok(text),
            Err(e) if started.elapsed() > Duration::from_secs(40) => return Err(e),
            Err(_) => tokio::time::sleep(Duration::from_secs(4)).await,
        }
    }
}

/// `c0mpute.toml` in a repository: what `git push` deploys.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Manifest {
    pub app: Option<AppSection>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AppSection {
    pub name: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_health")]
    pub health: String,
    pub state: Option<String>,
    #[serde(default = "default_snapshot")]
    pub snapshot_secs: u32,
    #[serde(default = "default_memory")]
    pub memory_mb: u32,
    #[serde(default = "default_cpu")]
    pub cpus: f64,
    #[serde(default = "default_replicas")]
    pub replicas: u8,
    #[serde(default)]
    pub domains: Vec<String>,
    /// A pinned registry image instead of building the repository.
    pub image: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn default_port() -> u16 {
    8080
}
fn default_health() -> String {
    "/".into()
}
fn default_snapshot() -> u32 {
    super::record::DEFAULT_SNAPSHOT_SECS
}
fn default_memory() -> u32 {
    256
}
fn default_cpu() -> f64 {
    0.5
}
fn default_replicas() -> u8 {
    super::record::DEFAULT_REPLICAS
}

impl AppSection {
    pub fn options(&self, dir: &Path, wait: Duration) -> DeployOptions {
        DeployOptions {
            name: self.name.clone(),
            source: match &self.image {
                Some(i) => Source::Pull(i.clone()),
                None => Source::Build(dir.to_path_buf()),
            },
            port: self.port,
            health_path: self.health.clone(),
            env: self.env.clone(),
            state_path: self.state.clone(),
            snapshot_secs: self.snapshot_secs,
            memory_mb: self.memory_mb,
            millicpus: (self.cpus * 1000.0).round() as u32,
            replicas: self.replicas,
            domains: self.domains.clone(),
            wait,
        }
    }
}

pub fn read_manifest(dir: &Path) -> Result<Option<AppSection>> {
    let p = dir.join("c0mpute.toml");
    if !p.exists() {
        return Ok(None);
    }
    let m: Manifest = toml::from_str(&std::fs::read_to_string(&p)?).context("c0mpute.toml")?;
    Ok(m.app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_defaults_and_options() {
        let m: Manifest = toml::from_str(
            r#"
            [app]
            name = "api"
            state = "/data"
            cpus = 1.5
            [app.env]
            NODE_ENV = "production"
        "#,
        )
        .unwrap();
        let a = m.app.unwrap();
        let o = a.options(Path::new("/src/api"), Duration::from_secs(60));
        assert_eq!(o.port, 8080);
        assert_eq!(o.millicpus, 1500);
        assert_eq!(o.replicas, 3);
        assert_eq!(o.state_path.as_deref(), Some("/data"));
        assert!(matches!(o.source, Source::Build(ref d) if d == Path::new("/src/api")));
        assert_eq!(
            o.env.get("NODE_ENV").map(String::as_str),
            Some("production")
        );
    }
}
