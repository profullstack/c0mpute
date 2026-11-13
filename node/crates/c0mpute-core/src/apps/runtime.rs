//! The container side of an app: load or pull its image, create the container
//! with hard limits, move state in and out, probe health.
//!
//! Everything goes through [`crate::container::command`] (docker, or whatever
//! `C0MPUTE_DOCKER` names) with argv only, never a shell. Containers get no
//! restart policy on purpose: the apps supervisor decides what runs, so a node
//! that comes back after losing primary never restarts a stale copy by itself.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::record::AppRecord;

/// Label every app container and volume carries (value: the app id).
pub const LABEL: &str = "c0mpute.app";

pub fn container_name(app_id: &str) -> String {
    format!("c0mpute-app-{app_id}")
}

pub fn volume_name(app_id: &str) -> String {
    format!("c0mpute-app-{app_id}-state")
}

async fn docker(args: &[String], stdin: Option<&[u8]>, timeout: Duration) -> Result<Vec<u8>> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = crate::container::command();
    cmd.args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .kill_on_drop(true);
    let mut child = cmd.spawn().context("run the container CLI")?;
    if let Some(input) = stdin
        && let Some(mut w) = child.stdin.take()
    {
        let input = input.to_vec();
        tokio::spawn(async move {
            let _ = w.write_all(&input).await;
            let _ = w.shutdown().await;
        });
    }
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .with_context(|| {
            format!(
                "docker {} timed out",
                args.first().map(String::as_str).unwrap_or("")
            )
        })??;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "docker {}: {}",
            args.first().map(String::as_str).unwrap_or(""),
            err.trim()
        );
    }
    Ok(out.stdout)
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Is `docker info` answering?
pub async fn available() -> bool {
    docker(
        &s(&["info", "--format", "{{.ServerVersion}}"]),
        None,
        Duration::from_secs(10),
    )
    .await
    .is_ok()
}

pub async fn image_present(reference: &str) -> bool {
    docker(
        &s(&["image", "inspect", "--format", "{{.Id}}", reference]),
        None,
        Duration::from_secs(20),
    )
    .await
    .is_ok()
}

pub async fn pull(reference: &str) -> Result<()> {
    docker(
        &s(&["pull", "--quiet", reference]),
        None,
        Duration::from_secs(900),
    )
    .await
    .map(|_| ())
}

/// `docker load` a (gzipped or plain) image tarball from a file.
pub async fn load(tar: &Path) -> Result<()> {
    docker(
        &[
            "load".into(),
            "--quiet".into(),
            "--input".into(),
            tar.display().to_string(),
        ],
        None,
        Duration::from_secs(900),
    )
    .await
    .map(|_| ())
}

/// The docker network guarded app containers join, and its Linux interface.
/// Not `container.run`'s jobs network: that one drops everything a container
/// sends to the host, replies included, and an app must answer the node's
/// proxy on its loopback-published port.
pub const APPS_NETWORK: &str = "c0mpute-apps";
pub const APPS_BRIDGE: &str = "c0apps0";

/// The egress guard for the apps bridge: `container.run`'s rules for private,
/// loopback, link-local (metadata), CGNAT and reserved ranges, and for the
/// host itself everything except replies to connections the host opened
/// (docker's loopback port publishing, which is how the proxy and the health
/// probe reach the app).
pub fn guard_rules(bridge: &str) -> Vec<Vec<String>> {
    let mut rules = crate::container_run::guard_rules(bridge);
    rules.retain(|r| r.first().map(String::as_str) != Some("INPUT"));
    rules.push(s(&["INPUT", "-i", bridge, "-m", "conntrack", "!", "--ctstate", "ESTABLISHED", "-j", "DROP"]));
    rules
}

/// Make sure the apps network exists with the egress guard on it. True when
/// app containers can be put behind the guard; false for a non-root worker
/// (it cannot change the firewall) or when docker or iptables refuse.
pub async fn ensure_guarded_network() -> bool {
    if !crate::container_run::is_root() {
        return false;
    }
    if let Err(e) = crate::container_run::ensure_bridge_network(APPS_NETWORK, APPS_BRIDGE, "c0mpute.apps=1").await {
        tracing::warn!(err = %format!("{e:#}"), "apps: could not create the apps network");
        return false;
    }
    crate::container_run::ensure_rules(guard_rules(APPS_BRIDGE)).await
}

/// Which network an app container joins: the guarded apps network when the
/// guard is in, docker's default bridge (`None`) when it is not, or an error
/// when the operator runs apps only behind the guard.
pub fn network_for(guarded: bool, required: bool) -> std::result::Result<Option<&'static str>, String> {
    match (guarded, required) {
        (true, _) => Ok(Some(APPS_NETWORK)),
        (false, true) => Err(
            "[apps] require_egress_guard is set and the egress guard is not in place (it needs a root worker, docker and iptables)".into(),
        ),
        (false, false) => Ok(None),
    }
}

/// Where and how tightly an app's container is created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CreateOpts {
    /// The docker network to join; `None` is docker's default bridge.
    pub network: Option<&'static str>,
    /// A hard quota on the container's writable layer, MiB, where the
    /// storage driver has one ([`quota_supported`]).
    pub layer_quota_mb: Option<u64>,
}

/// The `docker create` argv for an app. Pure, so it is tested without docker.
pub fn create_args(rec: &AppRecord, secret_env: &BTreeMap<String, String>, opts: &CreateOpts) -> Vec<String> {
    let name = container_name(&rec.app_id);
    let mut a: Vec<String> = vec![
        "create".into(),
        "--name".into(),
        name,
        "--init".into(),
        "--label".into(),
        format!("{LABEL}={}", rec.app_id),
        "--label".into(),
        format!("c0mpute.app.seq={}", rec.seq),
        "--memory".into(),
        format!("{}m", rec.memory_mb),
        "--memory-swap".into(),
        format!("{}m", rec.memory_mb),
        "--cpus".into(),
        format!("{:.3}", rec.millicpus as f64 / 1000.0),
        "--pids-limit".into(),
        "512".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--cap-drop".into(),
        "ALL".into(),
    ];
    // What ordinary images need to drop privileges and bind low ports, and
    // nothing that reaches the host.
    for cap in [
        "CHOWN",
        "SETUID",
        "SETGID",
        "DAC_OVERRIDE",
        "FOWNER",
        "NET_BIND_SERVICE",
    ] {
        a.push("--cap-add".into());
        a.push(cap.into());
    }
    a.extend(s(&[
        "--tmpfs",
        "/tmp:rw,nosuid,size=64m",
        "--log-opt",
        "max-size=10m",
        "--log-opt",
        "max-file=2",
        // Loopback only: the gateway is the way in.
        "--publish",
    ]));
    a.push(format!("127.0.0.1::{}", rec.port));
    if let Some(network) = opts.network {
        a.push("--network".into());
        a.push(network.into());
    }
    if let Some(mb) = opts.layer_quota_mb {
        a.push("--storage-opt".into());
        a.push(format!("size={mb}m"));
    }
    if let Some(p) = &rec.state_path {
        a.push("--volume".into());
        a.push(format!("{}:{}", volume_name(&rec.app_id), p));
    }
    a.push("--env".into());
    a.push(format!("PORT={}", rec.port));
    a.push("--env".into());
    a.push(format!("C0MPUTE_APP_ID={}", rec.app_id));
    for (k, v) in rec.env.iter().chain(secret_env.iter()) {
        a.push("--env".into());
        a.push(format!("{k}={v}"));
    }
    a.push(rec.image_ref().to_string());
    a
}

pub async fn create(rec: &AppRecord, secret_env: &BTreeMap<String, String>, opts: &CreateOpts) -> Result<()> {
    let _ = remove(&rec.app_id).await;
    let made = docker(&create_args(rec, secret_env, opts), None, Duration::from_secs(60)).await;
    if made.is_err() && opts.layer_quota_mb.is_some() {
        // The quota was detected, not promised: a daemon that refuses it
        // still runs the app, and the disk watchdog stays the limit.
        let _ = remove(&rec.app_id).await;
        let without = CreateOpts { layer_quota_mb: None, ..*opts };
        return docker(&create_args(rec, secret_env, &without), None, Duration::from_secs(60)).await.map(|_| ());
    }
    made.map(|_| ())
}

/// Does `docker create --storage-opt size=` work here? Only overlay2 on an
/// xfs filesystem mounted with project quotas enforces it; on ext4 (most
/// hosts) docker refuses the flag. `mount_options` are those of the
/// filesystem that holds docker's root directory.
pub fn quota_supported(driver: &str, backing_fs: &str, mount_options: &str) -> bool {
    driver == "overlay2"
        && backing_fs == "xfs"
        && mount_options.split(',').any(|o| matches!(o.trim(), "pquota" | "prjquota"))
}

/// The mount options of the filesystem holding `path`, from a
/// `/proc/self/mounts` listing (the longest mount point that is a prefix).
pub fn mount_options<'a>(mounts: &'a str, path: &Path) -> Option<&'a str> {
    mounts
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (_dev, point, _fs, options) = (f.next()?, f.next()?, f.next()?, f.next()?);
            path.starts_with(point).then_some((point.len(), options))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, options)| options)
}

/// What `docker info` says about where and how containers are stored.
#[derive(Clone, Debug, Default)]
struct Storage {
    root: Option<std::path::PathBuf>,
    quota: bool,
}

/// Asked once per process: the storage driver does not change under a
/// running node.
async fn storage() -> &'static Storage {
    static STORAGE: tokio::sync::OnceCell<Storage> = tokio::sync::OnceCell::const_new();
    STORAGE
        .get_or_init(|| async {
            let format = "{{.DockerRootDir}}\n{{.Driver}}\n{{range .DriverStatus}}{{index . 0}}={{index . 1}}\n{{end}}";
            let Ok(out) = docker(&s(&["info", "--format", format]), None, Duration::from_secs(20)).await else {
                return Storage::default();
            };
            let text = String::from_utf8_lossy(&out).into_owned();
            let mut lines = text.lines().map(str::trim);
            let root = lines.next().filter(|r| r.starts_with('/')).map(std::path::PathBuf::from);
            let driver = lines.next().unwrap_or("").to_string();
            let backing = lines.find_map(|l| l.strip_prefix("Backing Filesystem=")).unwrap_or("").to_string();
            let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
            let quota = root
                .as_deref()
                .and_then(|r| mount_options(&mounts, r))
                .is_some_and(|o| quota_supported(&driver, &backing, o));
            Storage { root, quota }
        })
        .await
}

/// The writable-layer quota to create an app with, MiB: its disk limit where
/// the storage driver enforces one, else none (the watchdog is the limit).
pub async fn layer_quota_mb(limit_bytes: u64) -> Option<u64> {
    (limit_bytes > 0 && storage().await.quota).then_some((limit_bytes >> 20).max(1))
}

/// Bytes the app has written inside its container (docker's own count of
/// the writable layer; the state volume is not in it).
pub async fn layer_bytes(app_id: &str) -> Option<u64> {
    let out = docker(
        &["inspect".into(), "--size".into(), "--format".into(), "{{.SizeRw}}".into(), container_name(app_id)],
        None,
        Duration::from_secs(60),
    )
    .await
    .ok()?;
    String::from_utf8_lossy(&out).trim().parse().ok()
}

/// Free bytes on the disk docker stores containers and volumes on, and the
/// free space `container.run` keeps on such a disk (its default floor).
pub async fn host_free() -> (u64, u64) {
    let Some(root) = storage().await.root.as_deref() else {
        return (u64::MAX, 0);
    };
    let (free, total) = c0mpute_store::free_space_at(root);
    (free, crate::container_run::min_free_bytes(&crate::container_run::ContainersConfig::default(), total))
}

/// Why an app is being stopped for the disk it uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskStop {
    pub reason: String,
    /// Remove the state volume as well. No rule asks for it today: an app
    /// over its cap is stopped with its data left on this node, because the
    /// part written since the last snapshot exists nowhere else and deleting
    /// it is the owner's call, not the node's.
    pub drop_state: bool,
}

/// The disk rule for one running app. `layer` is what it wrote inside its
/// container, `state` the size of its state directory (the last snapshot's
/// tar; anything over [`super::snapshot::MAX_STATE_BYTES`] when a snapshot
/// was refused for size). `limit` 0 means no per-app limit. `None`: fine.
pub fn disk_verdict(layer: u64, state: u64, limit: u64, free: u64, floor: u64) -> Option<DiskStop> {
    let mib = |b: u64| b >> 20;
    let state_cap = super::snapshot::MAX_STATE_BYTES as u64;
    let state_limit = if limit > 0 { limit.min(state_cap) } else { state_cap };
    if state > state_limit {
        return Some(DiskStop {
            reason: format!("its state directory is over {} MiB, the most an app may keep here; the data is kept on this node", mib(state_limit)),
            drop_state: false,
        });
    }
    if limit > 0 && layer.saturating_add(state) > limit {
        return Some(DiskStop {
            reason: format!(
                "it uses {} MiB of disk, over its {} MiB limit ({} MiB written inside the container, {} MiB of state)",
                mib(layer.saturating_add(state)),
                mib(limit),
                mib(layer),
                mib(state)
            ),
            drop_state: false,
        });
    }
    if free < floor {
        return Some(DiskStop {
            reason: format!("this node's docker disk is down to {} GiB free (it keeps {} GiB)", free >> 30, floor >> 30),
            drop_state: false,
        });
    }
    None
}

/// Did a snapshot fail because the state is over the size cap (and not
/// because docker did)?
pub fn over_state_cap(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("byte limit")
}

pub async fn start(app_id: &str) -> Result<()> {
    docker(
        &["start".into(), container_name(app_id)],
        None,
        Duration::from_secs(60),
    )
    .await
    .map(|_| ())
}

pub async fn remove(app_id: &str) -> Result<()> {
    docker(
        &["rm".into(), "--force".into(), container_name(app_id)],
        None,
        Duration::from_secs(60),
    )
    .await
    .map(|_| ())
}

pub async fn remove_volume(app_id: &str) -> Result<()> {
    docker(
        &[
            "volume".into(),
            "rm".into(),
            "--force".into(),
            volume_name(app_id),
        ],
        None,
        Duration::from_secs(60),
    )
    .await
    .map(|_| ())
}

pub async fn running(app_id: &str) -> bool {
    docker(
        &[
            "inspect".into(),
            "--format".into(),
            "{{.State.Running}}".into(),
            container_name(app_id),
        ],
        None,
        Duration::from_secs(20),
    )
    .await
    .map(|o| String::from_utf8_lossy(&o).trim() == "true")
    .unwrap_or(false)
}

/// The loopback port docker published for the app's port.
pub async fn host_port(app_id: &str, port: u16) -> Result<u16> {
    let out = docker(
        &["port".into(), container_name(app_id), format!("{port}/tcp")],
        None,
        Duration::from_secs(20),
    )
    .await?;
    parse_host_port(&String::from_utf8_lossy(&out)).context("docker published no loopback port")
}

pub fn parse_host_port(out: &str) -> Option<u16> {
    out.lines()
        .filter_map(|l| l.trim().rsplit_once(':'))
        .filter(|(h, _)| *h == "127.0.0.1" || *h == "0.0.0.0")
        .find_map(|(_, p)| p.trim().parse().ok())
}

/// A tar of the state directory, taken with the container paused so the
/// copy is crash-consistent (as if the power went out at that instant).
pub async fn snapshot_state(app_id: &str, state_path: &str) -> Result<Vec<u8>> {
    let name = container_name(app_id);
    let paused = docker(
        &["pause".into(), name.clone()],
        None,
        Duration::from_secs(30),
    )
    .await
    .is_ok();
    // Read through a cap: the app decides how much it writes to its state
    // path, and the whole tar used to be buffered before anyone looked at
    // its size (a 50 GiB state directory was 50 GiB of node memory).
    let out = docker_capped(
        &["cp".into(), format!("{name}:{state_path}"), "-".into()],
        Duration::from_secs(300),
        super::snapshot::MAX_STATE_BYTES,
    )
    .await;
    if paused {
        let _ = docker(&["unpause".into(), name], None, Duration::from_secs(30)).await;
    }
    out
}

/// Everything `reader` yields, or an error as soon as it has yielded more
/// than `max` bytes (never holding more than `max` plus one read buffer).
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut reader: R, max: usize) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(out);
        }
        if out.len() + n > max {
            bail!("output is over the {max} byte limit");
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// [`docker`] for output of a size the caller does not control: stdout is
/// read through [`read_capped`] and the command is killed when it runs over.
async fn docker_capped(args: &[String], timeout: Duration, max: usize) -> Result<Vec<u8>> {
    let what = args.first().map(String::as_str).unwrap_or("").to_string();
    let mut cmd = crate::container::command();
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn().context("run the container CLI")?;
    let stdout = child.stdout.take().context("container CLI stdout")?;
    let run = async {
        let bytes = read_capped(stdout, max).await;
        if bytes.is_err() {
            let _ = child.kill().await;
        }
        let status = child.wait().await?;
        let bytes = bytes.with_context(|| format!("docker {what}"))?;
        if !status.success() {
            bail!("docker {what} exited with {status}");
        }
        Ok(bytes)
    };
    tokio::time::timeout(timeout, run)
        .await
        .with_context(|| format!("docker {what} timed out"))?
}

/// The image that unpacks a snapshot into an app's state volume: busybox
/// 1.36, pinned by its multi-arch index digest.
pub const RESTORE_HELPER: &str =
    "busybox@sha256:73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662";

/// The `docker run` argv that unpacks a state tar (from [`snapshot_state`],
/// whose entries start with the state directory's own name) into the app's
/// volume. Pure, so it is tested without docker.
pub fn restore_args(app_id: &str) -> Vec<String> {
    vec![
        "run".into(),
        "--rm".into(),
        "--interactive".into(),
        "--network".into(),
        "none".into(),
        "--label".into(),
        format!("{LABEL}.restore={app_id}"),
        "--volume".into(),
        format!("{}:/restore", volume_name(app_id)),
        RESTORE_HELPER.into(),
        "tar".into(),
        "-x".into(),
        "-f".into(),
        "-".into(),
        "-C".into(),
        "/restore".into(),
        "--strip-components=1".into(),
    ]
}

/// Put a state tar back into the app's (fresh) state volume before its
/// container is created. Not `docker cp` into a created container: with the
/// containerd image store (Docker 29) a container that never started has no
/// filesystem to copy into ("destination must be a directory"). Docker
/// leaves a non-empty named volume alone when the app container mounts it,
/// so the app starts on exactly this state.
pub async fn restore_state(app_id: &str, tar: &[u8]) -> Result<()> {
    if !image_present(RESTORE_HELPER).await {
        pull(RESTORE_HELPER)
            .await
            .context("pull the restore helper image")?;
    }
    docker(&restore_args(app_id), Some(tar), Duration::from_secs(300))
        .await
        .map(|_| ())
}

pub async fn logs(app_id: &str, tail: u32) -> Result<String> {
    let mut cmd = crate::container::command();
    cmd.args([
        "logs",
        "--tail",
        &tail.min(5000).to_string(),
        "--timestamps",
        &container_name(app_id),
    ])
    .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output()).await??;
    // docker logs writes the app's stdout and stderr to ours.
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(text)
}

/// Every app container on this box (by label), for sweeping.
pub async fn labelled() -> Vec<String> {
    docker(
        &s(&[
            "ps",
            "--all",
            "--filter",
            &format!("label={LABEL}"),
            "--format",
            &format!("{{{{.Label \"{LABEL}\"}}}}"),
        ]),
        None,
        Duration::from_secs(20),
    )
    .await
    .map(|o| {
        String::from_utf8_lossy(&o)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// Probe `health_path` on the loopback port: any answer below 500 is healthy.
pub async fn healthy(host_port: u16, path: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    else {
        return false;
    };
    match client
        .get(format!("http://127.0.0.1:{host_port}{path}"))
        .send()
        .await
    {
        Ok(r) => r.status().as_u16() < 500,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::record::{AppRecord, tests::spec};

    #[tokio::test]
    async fn capped_read_stops_at_the_limit_instead_of_buffering_everything() {
        let small = read_capped(&b"state tar"[..], 1024).await.unwrap();
        assert_eq!(small, b"state tar");
        assert_eq!(read_capped(&[7u8; 1024][..], 1024).await.unwrap().len(), 1024, "exactly the limit is fine");
        // An endless stream is refused after `max` bytes, not read to the end.
        let endless = tokio::io::repeat(0u8);
        let err = read_capped(endless, 256 * 1024).await.unwrap_err();
        assert!(err.to_string().contains("over the 262144 byte limit"), "{err}");
    }

    #[test]
    fn create_args_are_locked_down() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let rec = AppRecord::sign(&kp, spec("demo"), 1).unwrap();
        let secret = BTreeMap::from([("DB_PASSWORD".to_string(), "hunter2".to_string())]);
        let a = create_args(&rec, &secret, &CreateOpts::default());
        let pair = |k: &str, v: &str| a.windows(2).any(|w| w[0] == k && w[1] == v);
        assert_eq!(a[0], "create");
        // Without the guard: docker's default bridge, as before, and no quota flag.
        assert!(!a.iter().any(|x| x == "--network" || x == "--storage-opt"));
        assert!(pair("--memory", "256m") && pair("--memory-swap", "256m"));
        assert!(pair("--cpus", "0.500"));
        assert!(pair("--cap-drop", "ALL"));
        assert!(pair("--security-opt", "no-new-privileges"));
        assert!(
            pair("--publish", "127.0.0.1::8080"),
            "only loopback is published"
        );
        assert!(pair(
            "--volume",
            &format!("{}:/data", volume_name(&rec.app_id))
        ));
        assert!(pair("--env", "DB_PASSWORD=hunter2"));
        assert!(pair("--env", "NODE_ENV=production"));
        assert_eq!(a.last().unwrap(), rec.image_ref());
        assert!(!a.iter().any(|x| x == "--privileged"
            || x.contains("network=host")
            || x == "--restart"
            || x == "sh"));
        assert!(!a.iter().any(|x| x.starts_with("SYS_") || x == "NET_ADMIN"));
    }

    #[test]
    fn guarded_apps_join_the_apps_network_and_stay_published_on_loopback() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let rec = AppRecord::sign(&kp, spec("demo"), 1).unwrap();
        let opts = CreateOpts { network: Some(APPS_NETWORK), layer_quota_mb: Some(2048) };
        let a = create_args(&rec, &BTreeMap::new(), &opts);
        let pair = |k: &str, v: &str| a.windows(2).any(|w| w[0] == k && w[1] == v);
        assert!(pair("--network", "c0mpute-apps"));
        assert!(pair("--storage-opt", "size=2048m"));
        // The proxy still reaches it the same way, and the image is still last.
        assert!(pair("--publish", "127.0.0.1::8080"));
        assert_eq!(a.last().unwrap(), rec.image_ref());
        assert_eq!(a.iter().filter(|x| *x == "--network").count(), 1);
    }

    #[test]
    fn the_guard_decides_the_network_and_require_refuses_without_it() {
        assert_eq!(network_for(true, false), Ok(Some(APPS_NETWORK)));
        assert_eq!(network_for(true, true), Ok(Some(APPS_NETWORK)));
        // No guard (a non-root worker): the default bridge, as before.
        assert_eq!(network_for(false, false), Ok(None));
        let err = network_for(false, true).unwrap_err();
        assert!(err.contains("require_egress_guard"), "{err}");
    }

    #[test]
    fn apps_guard_blocks_private_ranges_and_the_host_but_lets_replies_back() {
        let rules = guard_rules(APPS_BRIDGE);
        let has = |needle: &[&str]| rules.iter().any(|r| r.iter().map(String::as_str).eq(needle.iter().copied()));
        // The same forward rules container.run puts on its bridge.
        for range in crate::container_run::GUARDED_RANGES {
            assert!(has(&["DOCKER-USER", "-i", "c0apps0", "-d", range, "-j", "DROP"]), "{range}");
        }
        assert!(crate::container_run::GUARDED_RANGES.contains(&"169.254.0.0/16"), "cloud metadata");
        // To the host: only replies to connections the host opened (the
        // published loopback port); a blanket drop would cut the proxy off.
        assert!(has(&["INPUT", "-i", "c0apps0", "-m", "conntrack", "!", "--ctstate", "ESTABLISHED", "-j", "DROP"]));
        assert!(!has(&["INPUT", "-i", "c0apps0", "-j", "DROP"]));
        assert_eq!(rules.iter().filter(|r| r[0] == "INPUT").count(), 1);
        assert!(rules.iter().all(|r| r.contains(&"c0apps0".to_string()) && r.last().unwrap() == "DROP"));
    }

    #[test]
    fn layer_quota_only_on_overlay2_over_xfs_with_project_quotas() {
        assert!(quota_supported("overlay2", "xfs", "rw,relatime,attr2,inode64,prjquota"));
        assert!(quota_supported("overlay2", "xfs", "rw,pquota"));
        assert!(!quota_supported("overlay2", "xfs", "rw,relatime,noquota"));
        assert!(!quota_supported("overlay2", "extfs", "rw,relatime,prjquota"), "ext4, like most hosts");
        assert!(!quota_supported("overlayfs", "xfs", "rw,prjquota"), "the containerd image store");
        assert!(!quota_supported("", "", ""));
        let mounts = "/dev/sda1 / ext4 rw,relatime 0 0\n/dev/sdb1 /var/lib/docker xfs rw,noatime,prjquota 0 0\nproc /proc proc rw 0 0\n";
        assert_eq!(mount_options(mounts, Path::new("/var/lib/docker")), Some("rw,noatime,prjquota"));
        assert_eq!(mount_options(mounts, Path::new("/var/lib/docker/overlay2")), Some("rw,noatime,prjquota"));
        assert_eq!(mount_options(mounts, Path::new("/var/lib/dockerx")), Some("rw,relatime"), "a prefix of the name is not the mount");
        assert_eq!(mount_options("", Path::new("/var/lib/docker")), None);
    }

    #[test]
    fn disk_limit_arithmetic() {
        const MIB: u64 = 1 << 20;
        const GIB: u64 = 1 << 30;
        let limit = 2048 * MIB;
        let (free, floor) = (100 * GIB, 20 * GIB);
        // Under the limit with room on the host: nothing to do.
        assert_eq!(disk_verdict(500 * MIB, 900 * MIB, limit, free, floor), None);
        assert_eq!(disk_verdict(1024 * MIB, 1024 * MIB, limit, free, floor), None, "exactly the limit is fine");
        // Layer plus state over the limit: stopped, state kept.
        let stop = disk_verdict(1500 * MIB, 600 * MIB, limit, free, floor).unwrap();
        assert!(!stop.drop_state);
        assert!(stop.reason.contains("2100 MiB") && stop.reason.contains("2048 MiB limit"), "{}", stop.reason);
        // A state directory over what an app may keep (a snapshot refused
        // for size reports one byte more than the cap): stopped, and the
        // volume is kept, since what it holds past the last snapshot is the
        // only copy.
        let cap = crate::apps::snapshot::MAX_STATE_BYTES as u64;
        let stop = disk_verdict(0, cap + 1, limit, free, floor).unwrap();
        assert!(!stop.drop_state && stop.reason.contains("1024 MiB"), "{}", stop.reason);
        // An operator limit below the state cap caps the state too.
        assert!(!disk_verdict(0, 300 * MIB, 256 * MIB, free, floor).unwrap().drop_state);
        // No per-app limit (0): the state cap and the host floor still hold.
        assert_eq!(disk_verdict(50 * GIB, 0, 0, free, floor), None);
        assert!(disk_verdict(0, cap + 1, 0, free, floor).is_some());
        // The host under the floor container.run keeps: stopped, state kept.
        let stop = disk_verdict(MIB, MIB, limit, 19 * GIB, floor).unwrap();
        assert!(!stop.drop_state && stop.reason.contains("19 GiB free"), "{}", stop.reason);
        // Sizes that would overflow do not wrap into "fine".
        assert!(disk_verdict(u64::MAX, 1, limit, free, floor).is_some());
        // An unknown docker disk (free MAX, floor 0) never stops an app by itself.
        assert_eq!(disk_verdict(0, 0, limit, u64::MAX, 0), None);
    }

    #[test]
    fn a_snapshot_over_the_cap_is_told_from_a_docker_failure() {
        let over = anyhow::anyhow!("output is over the 1073741824 byte limit").context("docker cp");
        assert!(over_state_cap(&over));
        assert!(!over_state_cap(&anyhow::anyhow!("docker cp exited with exit status: 1")));
    }

    #[test]
    fn restore_unpacks_into_the_volume_offline() {
        let a = restore_args("abc");
        let pair = |k: &str, v: &str| a.windows(2).any(|w| w[0] == k && w[1] == v);
        assert_eq!(a[0], "run");
        assert!(pair(
            "--volume",
            &format!("{}:/restore", volume_name("abc"))
        ));
        assert!(pair("--network", "none"), "the helper has no network");
        assert!(a.iter().any(|x| x == RESTORE_HELPER) && RESTORE_HELPER.contains("@sha256:"));
        assert_eq!(a.last().unwrap(), "--strip-components=1");
    }

    #[test]
    fn parses_docker_port_output() {
        assert_eq!(parse_host_port("127.0.0.1:49153\n"), Some(49153));
        assert_eq!(parse_host_port("0.0.0.0:3000\n[::]:3000\n"), Some(3000));
        assert_eq!(parse_host_port(""), None);
    }
}
