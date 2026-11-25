//! The owner's side: build an image, publish it, sign the app record, hand
//! the secrets to the holders, and wait until a primary answers.
//!
//! The image never touches a registry: `docker save` → gzip → a hosting site
//! signed by the owner, so the network keeps three copies of it and any new
//! holder can load it while the owner is offline. A registry image pinned by
//! digest is the other option (`--pull`).
//!
//! The owner is the producer's account key ([`super::owner`]), so the same
//! name is the same app from any machine holding their c0mpute identity, and
//! the CLI joins the network as that key for owner-only calls. The app key
//! that encrypts state snapshots is derived from the identity per app name,
//! so every machine (and every redeploy) uses the same one and the network
//! can still read old snapshots.
//!
//! Secret env values live on the machine they were set on, in
//! `<data_dir>/apps/owner/<name>.json` (mode 0600), and sealed on the
//! holders; they are set again on another machine before deploying from it.
//! An app deployed under a machine key (before the account key) keeps its
//! random app key in that file, and its id, until `--move`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::owner::{self, AppOwner, KeyChoice, OwnerKind, Unlock};
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
    /// Peer ids allowed to hold the app (empty = any apps node).
    pub hosts: Vec<String>,
    pub wait: Duration,
    /// Which key signs (`--key`).
    pub key: KeyChoice,
    /// Hand an app owned by this machine's key to the account key (`--move`).
    pub moving: bool,
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

/// The machine-owned app a `--move` left running (an app name has no '.').
fn legacy_path(name: &str) -> PathBuf {
    owner_dir().join(format!("{name}.machine.json"))
}

fn load_state(path: &Path) -> OwnerState {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn load_owner(name: &str) -> OwnerState {
    load_state(&owner_path(name))
}

/// What this machine kept of the machine-owned app named `name` after it
/// was moved to the account key (empty once that app is destroyed).
pub fn load_legacy(name: &str) -> OwnerState {
    load_state(&legacy_path(name))
}

fn save_legacy(name: &str, st: &OwnerState) -> Result<()> {
    std::fs::create_dir_all(owner_dir())?;
    super::write_private(&legacy_path(name), &serde_json::to_vec_pretty(st)?)?;
    Ok(())
}

/// The secrets a deploy by `owner` hands to holders, updating `st` to match.
/// An account owner's app key is derived from the identity (the same on
/// every machine); a machine owner's is random and kept in `st`. The secret
/// env is whatever this machine has set.
fn plan_secrets(st: &mut OwnerState, owner: &AppOwner, name: &str, has_state: bool) -> Option<AppSecrets> {
    match owner.app_key(name) {
        Some(app_key) => {
            let env = st.secrets.take().map(|s| s.env).unwrap_or_default();
            if has_state || !env.is_empty() {
                st.secrets = Some(AppSecrets { app_key, env });
            }
        }
        // The app key outlives every deploy; secrets are whatever is set now.
        None if has_state && st.secrets.is_none() => {
            st.secrets = Some(AppSecrets::generate(BTreeMap::new()));
        }
        None => {}
    }
    st.secrets.clone().filter(|s| has_state || !s.env.is_empty())
}

/// Would deploying `ours` drop secret env the live app `live` has? True when
/// the live record names sealed secrets that are not the ones this machine
/// would send and this machine has no secret env of its own to send: they
/// were set on another machine and cannot be read back from the network.
fn drops_live_secrets(live: &AppRecord, ours: Option<&AppSecrets>, bare: &AppSecrets) -> bool {
    let Some(live_hash) = live.secrets_hash.as_deref() else {
        return false;
    };
    let has_env = ours.is_some_and(|s| !s.env.is_empty());
    !live.deleted && !has_env && live_hash != bare.hash()
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
///
/// Holders take sealed secrets and log requests only from the owner's
/// libp2p peer, so an account owner joins *as* the account key: the same
/// peer from every machine holding the identity, with nothing changed on
/// the holders. A machine owner joins as this machine's buyer identity.
pub async fn owner_node(owner: &AppOwner) -> Result<(crate::Supervisor, Hosting, Apps)> {
    let supervisor = match owner.kind {
        OwnerKind::Machine => crate::Supervisor::boot_buyer(crate::Config::default()).await?,
        OwnerKind::Account => {
            let dir = crate::config::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("apps")
                .join("account-node");
            crate::Supervisor::boot_with_keypair(crate::Config::default(), dir, owner.keypair.clone()).await?
        }
    };
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

/// Deploy (or redeploy) an app and wait until it answers. `unlock` opens the
/// c0mpute identity when the account key is needed.
pub async fn deploy(opts: DeployOptions, unlock: Unlock<'_>) -> Result<AppRecord> {
    if !site::valid_name(&opts.name) || opts.name.len() > 59 {
        bail!("app name must be 1-59 of a-z, 0-9 and '-'");
    }
    let mut st = load_owner(&opts.name);
    let owner = owner::choose(
        &opts.name,
        opts.key,
        opts.moving,
        st.record.as_ref(),
        AppOwner::machine()?,
        unlock,
    )?;
    // A move: the machine-owned app is set aside (it keeps running under its
    // own id until it is destroyed) and its secret env carries over.
    let first_here = st.record.is_none();
    let moved_from = st.record.take().filter(|r| !owner.owns(r));
    let legacy = moved_from.as_ref().map(|r| OwnerState {
        secrets: st.secrets.clone(),
        record: Some(r.clone()),
    });
    let keypair = &owner.keypair;
    let seq = now_ms();
    let secrets = plan_secrets(&mut st, &owner, &opts.name, opts.state_path.is_some());

    let (_sup, h, apps) = owner_node(&owner).await?;
    // Deploying an account-owned app from a machine that never deployed it:
    // never quietly drop the secret env another machine set.
    if first_here
        && let Some(app_key) = owner.app_key(&opts.name)
        && let Some(live) = apps.lookup_record(&owner.app_id(&opts.name)).await
    {
        let bare = AppSecrets { app_key, env: BTreeMap::new() };
        if drops_live_secrets(&live, secrets.as_ref(), &bare) {
            bail!(
                "{} ({}) is live with secrets that were set on another machine, and none are set here. \
                 Secret values cannot be read back from the network: set them on this machine first \
                 (`c0mpute apps secrets set {} KEY=VALUE ...`), then deploy; what is set here replaces them",
                opts.name,
                live.app_id,
                opts.name
            );
        }
        if live.secrets_hash.is_some() && live.secrets_hash != secrets.as_ref().map(|s| s.hash()) && !live.deleted {
            eprintln!(
                "note: the secrets set on this machine differ from the ones {} runs with; these replace them",
                opts.name
            );
        }
    }
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
            if size > site::MAX_SMALL_SITE_BYTES {
                let _ = std::fs::remove_dir_all(&tmp);
                bail!(
                    "the image is {} MiB gzipped; the limit is {} MiB (use a slim base image, or --pull a pinned registry image)",
                    size >> 20,
                    site::MAX_SMALL_SITE_BYTES >> 20
                );
            }
            let site_name = format!("app-{}", opts.name);
            let built = site::build_from_dir(&tmp, &site_name)?;
            let _ = std::fs::remove_dir_all(&tmp);
            let manifest = built.manifest_hash();
            let rec = site::SiteRecord::sign(
                keypair,
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
        hosts: opts.hosts.clone(),
    };
    let rec = AppRecord::sign(keypair, spec, seq)?;
    if let Some(s) = &secrets {
        apps.own_secrets(&rec.app_id, s.clone()).await;
    }
    st.record = Some(rec.clone());
    if let Some(l) = &legacy {
        save_legacy(&opts.name, l)?;
    }
    save_owner(&opts.name, &st)?;
    apps.publish_record(rec.clone()).await?;
    eprintln!(
        "app {} is {} (seq {}), owned by {} ({}…)",
        rec.name,
        rec.app_id,
        rec.seq,
        owner.kind.label(),
        &rec.owner[..12.min(rec.owner.len())]
    );

    let up = wait_until_up(&apps, Some(&h), image_site.as_ref(), &rec, opts.wait).await;
    if let Some(old) = moved_from {
        retire_moved(&apps, &old, &rec, up).await;
    }
    Ok(rec)
}

/// After a `--move`: take the machine-owned app down, unless that would
/// lose something. State does not follow an app to a new id, and an app
/// that is not up yet has nothing to take over, so then the old one is left
/// running for the owner to delete (`apps destroy <name> --key machine`).
async fn retire_moved(apps: &Apps, old: &AppRecord, new: &AppRecord, up: bool) {
    let name = &new.name;
    eprintln!(
        "moved: {name} is now {} under your account key (was {}). Repoint its custom domains: \
         TXT _c0mpute-app.<domain> = \"app={}\". Node operators that list owner keys need the new one: {}",
        new.app_id, old.app_id, new.app_id, new.owner
    );
    let keep = if old.state_path.is_some() {
        Some("it holds state, and state does not move to a new app id by itself: copy your data over first")
    } else if !up {
        Some("the new app is not answering yet")
    } else {
        None
    };
    if let Some(why) = keep {
        eprintln!(
            "the old app {} is still running ({why}). When you are done with it: \
             `c0mpute apps destroy {name} --key machine --yes`",
            old.app_id
        );
        return;
    }
    let tomb = hosting::publisher_keypair().and_then(|k| AppRecord::tombstone(&k, old, now_ms()));
    match tomb {
        Ok(tomb) => {
            for _ in 0..3 {
                let _ = apps.publish_record(tomb.clone()).await;
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            let _ = std::fs::remove_file(legacy_path(name));
            eprintln!("the old app {} is deleted", old.app_id);
        }
        Err(e) => eprintln!(
            "could not delete the old app {} ({e:#}); run `c0mpute apps destroy {name} --key machine --yes`",
            old.app_id
        ),
    }
}

/// Keep gossiping (records, secrets, the image) until a primary is healthy
/// on the current version with every copy in place, or `wait` passes.
/// Returns whether a healthy primary answered on this version.
pub async fn wait_until_up(
    apps: &Apps,
    h: Option<&Hosting>,
    site: Option<&site::SiteRecord>,
    rec: &AppRecord,
    wait: Duration,
) -> bool {
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
            return true;
        }
    }
    eprintln!(
        "still converging after {} s; the network keeps going without this machine",
        wait.as_secs()
    );
    false
}

fn short(p: &str) -> &str {
    &p[p.len().saturating_sub(8)..]
}

/// Delete an app: a signed tombstone, gossiped for a while. With the
/// account key this works from any machine holding the identity: the record
/// to tombstone is fetched from the network when this machine has none.
pub async fn destroy(name: &str, key: KeyChoice, unlock: Unlock<'_>) -> Result<String> {
    if !site::valid_name(name) {
        bail!("app name must be 1-63 of a-z, 0-9 and '-'");
    }
    let machine = AppOwner::machine()?;
    let current = load_owner(name).record;
    let legacy_rec = load_legacy(name).record.filter(|r| machine.owns(r));
    if key == KeyChoice::Auto && current.is_none() && legacy_rec.is_some() {
        bail!(
            "the only app named {name} deployed from here is the machine-owned one a --move left running: \
             pass --key machine to delete it"
        );
    }
    let (owner, app_id) = owner::resolve(name, key, current.as_ref(), legacy_rec.as_ref(), machine, unlock)?;
    // The file that goes with the app being deleted.
    let (prev, file) = match (legacy_rec, current) {
        (Some(r), _) if r.app_id == app_id => (Some(r), Some(legacy_path(name))),
        (_, Some(r)) if r.app_id == app_id => (Some(r), Some(owner_path(name))),
        _ => (None, None),
    };
    let (_sup, _h, apps) = owner_node(&owner).await?;
    let prev = match prev {
        Some(r) => r,
        None => apps
            .lookup_record(&app_id)
            .await
            .filter(|r| !r.deleted && owner.owns(r))
            .with_context(|| {
                format!(
                    "no live app named {name} is owned by {} ({app_id}), and none was deployed under that \
                     name from this machine",
                    owner.kind.label()
                )
            })?,
    };
    let tomb = AppRecord::tombstone(&owner.keypair, &prev, now_ms())?;
    for _ in 0..6 {
        apps.publish_record(tomb.clone()).await?;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    if let Some(f) = file {
        let _ = std::fs::remove_file(f);
    }
    Ok(tomb.app_id)
}

/// Owner: read the primary's logs. Holders answer the owner's peer only, so
/// the node joins as the key that owns the app ([`owner_node`]).
pub async fn logs(name_or_id: &str, tail: u32, key: KeyChoice, unlock: Unlock<'_>) -> Result<String> {
    let st = load_owner(name_or_id);
    let legacy = load_legacy(name_or_id);
    let (owner, id) = owner::resolve(
        name_or_id,
        key,
        st.record.as_ref(),
        legacy.record.as_ref(),
        AppOwner::machine()?,
        unlock,
    )?;
    let (_sup, _h, apps) = owner_node(&owner).await?;
    // Hear at least one round of node announcements first.
    let started = Instant::now();
    loop {
        match apps.logs(&id, tail).await {
            Ok(text) => return Ok(text),
            Err(e) if started.elapsed() > Duration::from_secs(40) => {
                // Say so when the refusal is about who is asking.
                return match apps.record(&id).await {
                    Some(r) if !owner.owns(&r) => Err(e.context(format!(
                        "{} ({id}) is owned by another key ({}…), not {} ({}…): only its owner reads its logs \
                         (--key account|machine picks the key)",
                        r.name,
                        &r.owner[..12.min(r.owner.len())],
                        owner.kind.label(),
                        &owner.owner_hex[..12.min(owner.owner_hex.len())]
                    ))),
                    _ => Err(e),
                };
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(4)).await,
        }
    }
}

/// The app id `name_or_id` means on this machine, for read-only lookups
/// (`apps status`): what was deployed from here, else an id as given, else
/// the id the name has under the account key when the identity opens.
pub fn app_id_of(name_or_id: &str, unlock: Unlock<'_>) -> String {
    if let Some(r) = load_owner(name_or_id).record {
        return r.app_id;
    }
    if super::record::valid_app_id(name_or_id) || !site::valid_name(name_or_id) {
        return name_or_id.to_string();
    }
    match unlock() {
        Ok(Ok(k)) => AppOwner::account(k)
            .map(|a| a.app_id(name_or_id))
            .unwrap_or_else(|_| name_or_id.to_string()),
        _ => name_or_id.to_string(),
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
    /// Peer ids allowed to hold the app (empty = any apps node).
    #[serde(default)]
    pub hosts: Vec<String>,
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
            hosts: self.hosts.clone(),
            wait,
            key: KeyChoice::Auto,
            moving: false,
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
        assert_eq!((o.key, o.moving), (KeyChoice::Auto, false));
    }

    fn account(seed: u8) -> AppOwner {
        AppOwner::account(c0mpute_secure_chat::ChatKey::from_seed([seed; 32])).unwrap()
    }

    fn machine() -> AppOwner {
        AppOwner::from_machine_key(c0mpute_net::libp2p::identity::Keypair::generate_ed25519()).unwrap()
    }

    #[test]
    fn an_account_app_has_the_same_secrets_on_every_machine() {
        // Machine A and machine B hold the same identity and set the same
        // secret env: the sealed blob (and so the record's hash) is the same,
        // and the app key is never a random one.
        let env = BTreeMap::from([("DB".to_string(), "x".to_string())]);
        // Each once set the secret before deploying: a random key is on disk.
        let mut a = OwnerState {
            secrets: Some(AppSecrets::generate(env.clone())),
            record: None,
        };
        let mut b = OwnerState {
            secrets: Some(AppSecrets::generate(env.clone())),
            record: None,
        };
        let sa = plan_secrets(&mut a, &account(7), "api", true).unwrap();
        let sb = plan_secrets(&mut b, &account(7), "api", true).unwrap();
        assert_eq!(sa, sb);
        assert_eq!(sa.hash(), sb.hash());
        assert_eq!(Some(sa.app_key.clone()), account(7).app_key("api"));
        assert_eq!(sa.env, env);
        assert_eq!(a.secrets.as_ref(), Some(&sa), "what is kept is what was sent");
        // A machine with nothing set still rebuilds the app key for state.
        let mut c = OwnerState::default();
        let sc = plan_secrets(&mut c, &account(7), "api", true).unwrap();
        assert_eq!(sc.app_key, sa.app_key);
        assert!(sc.env.is_empty());
        // No state and no secret env: nothing sealed, nothing kept.
        let mut d = OwnerState::default();
        assert!(plan_secrets(&mut d, &account(7), "api", false).is_none());
        assert!(d.secrets.is_none());
    }

    #[test]
    fn a_machine_owned_app_keeps_its_stored_app_key() {
        let m = machine();
        let mut st = OwnerState::default();
        let first = plan_secrets(&mut st, &m, "api", true).unwrap();
        let again = plan_secrets(&mut st, &m, "api", true).unwrap();
        assert_eq!(first, again, "the random key outlives every deploy");
        // Without state or secret env there is nothing to seal.
        let mut none = OwnerState::default();
        assert!(plan_secrets(&mut none, &m, "api", false).is_none());
    }

    #[test]
    fn a_move_carries_the_secret_env_under_the_derived_key() {
        let env = BTreeMap::from([("TOKEN".to_string(), "t".to_string())]);
        let mut st = OwnerState {
            secrets: Some(AppSecrets::generate(env.clone())),
            record: None,
        };
        let old_key = st.secrets.as_ref().unwrap().app_key.clone();
        let s = plan_secrets(&mut st, &account(7), "api", false).unwrap();
        assert_eq!(s.env, env);
        assert_ne!(s.app_key, old_key);
        assert_eq!(Some(s.app_key), account(7).app_key("api"));
    }

    #[test]
    fn a_first_deploy_from_another_machine_never_drops_live_secrets() {
        use super::super::record::tests::spec;
        let a = account(7);
        let key = a.app_key("api").unwrap();
        let bare = AppSecrets { app_key: key.clone(), env: BTreeMap::new() };
        let with_env = AppSecrets {
            app_key: key,
            env: BTreeMap::from([("DB".to_string(), "x".to_string())]),
        };
        let live = |s: &AppSecrets| {
            let mut sp = spec("api");
            sp.secrets_hash = Some(s.hash());
            AppRecord::sign(&a.keypair, sp, 1).unwrap()
        };
        // Live with secret env, none here: refused.
        assert!(drops_live_secrets(&live(&with_env), Some(&bare), &bare));
        assert!(drops_live_secrets(&live(&with_env), None, &bare));
        // The same env set here, or a different one on purpose: allowed.
        assert!(!drops_live_secrets(&live(&with_env), Some(&with_env), &bare));
        // Live with only the app key (state, no secret env): nothing to lose.
        assert!(!drops_live_secrets(&live(&bare), Some(&bare), &bare));
        // No sealed secrets at all, or a deleted app.
        let mut sp = spec("api");
        sp.secrets_hash = None;
        sp.state_path = None;
        assert!(!drops_live_secrets(&AppRecord::sign(&a.keypair, sp, 1).unwrap(), None, &bare));
        let tomb = AppRecord::tombstone(&a.keypair, &live(&with_env), 2).unwrap();
        assert!(!drops_live_secrets(&tomb, None, &bare));
    }
}
