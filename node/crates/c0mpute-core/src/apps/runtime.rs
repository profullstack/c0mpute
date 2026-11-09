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

/// The `docker create` argv for an app. Pure, so it is tested without docker.
pub fn create_args(rec: &AppRecord, secret_env: &BTreeMap<String, String>) -> Vec<String> {
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

pub async fn create(rec: &AppRecord, secret_env: &BTreeMap<String, String>) -> Result<()> {
    let _ = remove(&rec.app_id).await;
    docker(&create_args(rec, secret_env), None, Duration::from_secs(60))
        .await
        .map(|_| ())
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
    let out = docker(
        &["cp".into(), format!("{name}:{state_path}"), "-".into()],
        None,
        Duration::from_secs(300),
    )
    .await;
    if paused {
        let _ = docker(&["unpause".into(), name], None, Duration::from_secs(30)).await;
    }
    out
}

/// Put a state tar (from [`snapshot_state`]) back into a created container,
/// before it starts.
pub async fn restore_state(app_id: &str, state_path: &str, tar: &[u8]) -> Result<()> {
    let parent = Path::new(state_path)
        .parent()
        .map(|p| p.display().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "/".into());
    docker(
        &[
            "cp".into(),
            "-".into(),
            format!("{}:{parent}", container_name(app_id)),
        ],
        Some(tar),
        Duration::from_secs(300),
    )
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

    #[test]
    fn create_args_are_locked_down() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let rec = AppRecord::sign(&kp, spec("demo"), 1).unwrap();
        let secret = BTreeMap::from([("DB_PASSWORD".to_string(), "hunter2".to_string())]);
        let a = create_args(&rec, &secret);
        let pair = |k: &str, v: &str| a.windows(2).any(|w| w[0] == k && w[1] == v);
        assert_eq!(a[0], "create");
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
    fn parses_docker_port_output() {
        assert_eq!(parse_host_port("127.0.0.1:49153\n"), Some(49153));
        assert_eq!(parse_host_port("0.0.0.0:3000\n[::]:3000\n"), Some(3000));
        assert_eq!(parse_host_port(""), None);
    }
}
