//! `c0mpute build`, `c0mpute run` and `c0mpute containers` (DIP 0033):
//! offload a docker build, or a command in a container, to a trusted builder
//! on the network and get the image or the output files back. No git: the
//! input is a tarball of any directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use c0mpute_core::config::Config;
use c0mpute_core::container_run::{self, BuildSpec, Progress, Request};
use clap::{Args, Subcommand};

/// Who runs the job and with what limits (shared by build and run).
#[derive(Args, Debug, Clone)]
pub struct Target {
    /// Builder to send the job to: a peer id, or a multiaddr ending in
    /// /p2p/<peer id> (repeat for several). Defaults to `[containers]
    /// builders` and `C0MPUTE_BUILDERS`.
    #[arg(long = "peer", value_name = "PEER_ID|MULTIADDR")]
    pub peers: Vec<String>,
    /// Accept any builder on the network. A builder controls the bytes it
    /// returns: use only for throwaway work.
    #[arg(long)]
    pub any_worker: bool,
    /// CPUs for a run step (builds use the builder's BuildKit).
    #[arg(long)]
    pub cpus: Option<f64>,
    /// Memory for a run step, MiB.
    #[arg(long)]
    pub memory_mb: Option<u64>,
    /// Scratch disk the job may use on the builder, MiB.
    #[arg(long)]
    pub disk_mb: Option<u64>,
    /// Give up after this many seconds (build + run).
    #[arg(long, default_value_t = 1800)]
    pub timeout_secs: u64,
    /// No network inside the build / run.
    #[arg(long)]
    pub no_network: bool,
    /// Highest price in USD you will pay (your own builders bid 0).
    #[arg(long, default_value_t = 0.0)]
    pub max_price: f64,
    /// Print the result JSON on stdout.
    #[arg(long)]
    pub json: bool,
}

/// `c0mpute build`.
#[derive(Args, Debug)]
pub struct BuildArgs {
    /// Build context: a directory (packed with its .dockerignore applied) or
    /// a tar / tar.gz you made (e.g. `git archive --format=tar.gz HEAD`).
    pub context: PathBuf,
    /// Dockerfile path inside the context.
    #[arg(short = 'f', long = "file", default_value = "Dockerfile")]
    pub dockerfile: String,
    /// Build-time variable, KEY=VALUE (repeatable).
    #[arg(long = "build-arg", value_name = "KEY=VALUE")]
    pub build_args: Vec<String>,
    /// Build stage to stop at.
    #[arg(long)]
    pub target: Option<String>,
    /// Write the image (docker save | gzip) here.
    #[arg(short = 'o', long = "output")]
    pub output: Option<PathBuf>,
    /// Load the image into the local docker (`docker load`).
    #[arg(long)]
    pub load: bool,
    /// Tag the loaded image (repeatable; implies --load).
    #[arg(short = 't', long = "tag")]
    pub tags: Vec<String>,
    #[command(flatten)]
    pub target_opts: Target,
}

/// `c0mpute run`.
#[derive(Args, Debug)]
pub struct RunArgs {
    /// Digest-pinned image to run in (name@sha256:...).
    #[arg(long, conflicts_with = "build")]
    pub image: Option<String>,
    /// Build this directory (or tarball) first and run in the result.
    #[arg(long)]
    pub build: Option<PathBuf>,
    #[arg(short = 'f', long = "file", default_value = "Dockerfile")]
    pub dockerfile: String,
    #[arg(long = "build-arg", value_name = "KEY=VALUE")]
    pub build_args: Vec<String>,
    #[arg(long)]
    pub target: Option<String>,
    /// Directory (or tarball) copied into the workdir before the command.
    #[arg(long = "in")]
    pub input: Option<PathBuf>,
    /// Path to bring back, relative to the workdir or absolute (repeatable).
    #[arg(long = "out")]
    pub outputs: Vec<String>,
    /// Where outputs are unpacked.
    #[arg(long, default_value = ".")]
    pub dest: PathBuf,
    /// Environment variable, KEY=VALUE (repeatable). Visible to the builder.
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// Working directory in the container.
    #[arg(long, default_value = "/work")]
    pub workdir: String,
    #[command(flatten)]
    pub target_opts: Target,
    /// The command to run.
    #[arg(last = true, required = true)]
    pub cmd: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum ContainersCmd {
    /// The `[containers]` settings and this machine's build identity.
    Show,
    /// Print the peer id builders must trust for `c0mpute build` from here.
    Id,
    /// Trust a buyer peer id to run jobs on this node (worker side).
    Allow { peer_id: String },
    /// Stop trusting a buyer peer id.
    Deny { peer_id: String },
    /// Serve any buyer (not recommended: a build runs the buyer's Dockerfile).
    Open,
    /// Serve trusted buyers only (the default).
    Close,
    /// Add a builder this machine sends jobs to (buyer side).
    AddBuilder { peer_id: String },
    /// Remove a builder.
    RmBuilder { peer_id: String },
}

fn kv(list: &[String], what: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for item in list {
        let (k, v) = item
            .split_once('=')
            .with_context(|| format!("{what} {item:?} must be KEY=VALUE"))?;
        out.insert(k.to_string(), v.to_string());
    }
    Ok(out)
}

fn unique() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

fn scratch() -> Result<PathBuf> {
    let dir = container_run::identity_dir().join("tmp");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A directory is packed (into scratch, removed afterwards); a file is used as is.
fn tarball(
    path: &Path,
    dockerignore: bool,
    progress: &Progress,
    temps: &mut Vec<PathBuf>,
) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let dest = scratch()?.join(format!("ctx-{}.tar.gz", unique()));
    let size = container_run::pack_dir(path, &dest, dockerignore)?;
    progress(&format!("packed {} ({} KiB)", path.display(), size >> 10));
    temps.push(dest.clone());
    Ok(dest)
}

fn progress_fn() -> Progress {
    Arc::new(|line: &str| eprintln!("  {line}"))
}

fn builders(cfg: &Config, t: &Target) -> Vec<String> {
    if !t.peers.is_empty() {
        return t.peers.clone();
    }
    cfg.containers.builders_with_env()
}

async fn submit(cfg: Config, req: Request, progress: Progress) -> Result<container_run::Outcome> {
    let id_dir = container_run::identity_dir();
    let _lock = container_run::lock_identity(&id_dir, &progress)?;
    let supervisor = c0mpute_core::Supervisor::boot_with_identity(cfg, id_dir).await?;
    progress(&format!(
        "buyer {} joining the network",
        supervisor.libp2p.peer_id()
    ));
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let out_dir = scratch()?.join(format!("job-{}", unique()));
    container_run::submit(
        supervisor.libp2p.clone(),
        supervisor.store.clone(),
        req,
        &out_dir,
        progress,
    )
    .await
}

fn summarize(outcome: &container_run::Outcome) {
    let r = &outcome.result;
    let secs = r["duration_ms"].as_u64().unwrap_or(0) / 1000;
    eprintln!(
        "{} on {} in {secs}s (job {}){}",
        if outcome.ok() { "ok" } else { "FAILED" },
        outcome.worker,
        outcome.job_id,
        r["error"]
            .as_str()
            .map(|e| format!(": {e}"))
            .unwrap_or_default()
    );
}

fn print_tail_if_failed(outcome: &container_run::Outcome) {
    if outcome.ok() {
        return;
    }
    for key in ["build_log_tail", "log_tail"] {
        if let Some(t) = outcome.result[key].as_str().filter(|t| !t.is_empty()) {
            eprintln!("--- {key} ---\n{t}");
        }
    }
}

fn docker(args: &[&str]) -> Result<String> {
    let out = c0mpute_core::container::command_sync()
        .args(args)
        .output()
        .context("run docker")?;
    if !out.status.success() {
        bail!(
            "docker {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub async fn run_build(args: BuildArgs, config_path: &Path) -> Result<()> {
    let cfg = Config::load_or_default(config_path)?;
    let progress = progress_fn();
    let mut temps = Vec::new();
    let context = tarball(&args.context, true, &progress, &mut temps)?;
    let req = Request {
        context: Some(context),
        build: Some(BuildSpec {
            dockerfile: args.dockerfile,
            build_args: kv(&args.build_args, "--build-arg")?,
            target: args.target,
        }),
        cpus: args.target_opts.cpus,
        memory_mb: args.target_opts.memory_mb,
        disk_mb: args.target_opts.disk_mb,
        timeout_secs: Some(args.target_opts.timeout_secs),
        network: Some(!args.target_opts.no_network),
        builders: builders(&cfg, &args.target_opts),
        any_worker: args.target_opts.any_worker,
        max_price_usd: args.target_opts.max_price,
        ..Default::default()
    };
    let result = submit(cfg, req, progress.clone()).await;
    for t in temps {
        let _ = std::fs::remove_file(t);
    }
    let outcome = result?;
    summarize(&outcome);
    print_tail_if_failed(&outcome);
    if args.target_opts.json {
        println!("{}", serde_json::to_string_pretty(&outcome.result)?);
    }
    if !outcome.ok() {
        std::process::exit(1);
    }
    let image = outcome
        .image
        .clone()
        .context("the builder returned no image")?;
    let id = outcome.result["image"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let load = args.load || !args.tags.is_empty();
    if load {
        progress("docker load");
        let loaded = docker(&["load", "--input", &image.display().to_string()])?;
        // The saved image carries the builder's job tag; swap it for ours.
        let job_tag = loaded
            .lines()
            .find_map(|l| l.strip_prefix("Loaded image: "))
            .map(str::to_string);
        // Tag from the loaded name: the id a builder reports can differ
        // from this daemon's when the two use different image stores.
        let source = job_tag.clone().unwrap_or_else(|| id.clone());
        for tag in &args.tags {
            docker(&["tag", &source, tag])?;
        }
        if let Some(t) = job_tag
            && t.starts_with("c0mpute-job/")
            && !args.tags.is_empty()
        {
            let _ = docker(&["image", "rm", &t]);
        }
        eprintln!(
            "loaded {id}{}",
            if args.tags.is_empty() {
                String::new()
            } else {
                format!(" as {}", args.tags.join(", "))
            }
        );
    }
    match &args.output {
        Some(dest) => {
            if std::fs::rename(&image, dest).is_err() {
                std::fs::copy(&image, dest).with_context(|| format!("write {}", dest.display()))?;
                let _ = std::fs::remove_file(&image);
            }
            eprintln!("image written to {}", dest.display());
        }
        None if load => {
            let _ = std::fs::remove_file(&image);
        }
        None => eprintln!("image at {} (pass -o or --load next time)", image.display()),
    }
    if let Some(dir) = image.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    if !args.target_opts.json {
        println!("{id}");
    }
    Ok(())
}

pub async fn run_run(args: RunArgs, config_path: &Path) -> Result<()> {
    let cfg = Config::load_or_default(config_path)?;
    let progress = progress_fn();
    let mut temps = Vec::new();
    if args.image.is_none() && args.build.is_none() {
        bail!("give --image <name@sha256:...> or --build <dir>");
    }
    let context = match &args.build {
        Some(dir) => Some(tarball(dir, true, &progress, &mut temps)?),
        None => None,
    };
    let input = match &args.input {
        Some(dir) => Some(tarball(dir, false, &progress, &mut temps)?),
        None => None,
    };
    let req = Request {
        context,
        build: args.build.as_ref().map(|_| BuildSpec {
            dockerfile: args.dockerfile.clone(),
            build_args: kv(&args.build_args, "--build-arg").unwrap_or_default(),
            target: args.target.clone(),
        }),
        image: args.image.clone(),
        cmd: args.cmd.clone(),
        input,
        env: kv(&args.env, "--env")?,
        workdir: Some(args.workdir.clone()),
        outputs: args.outputs.clone(),
        cpus: args.target_opts.cpus,
        memory_mb: args.target_opts.memory_mb,
        disk_mb: args.target_opts.disk_mb,
        timeout_secs: Some(args.target_opts.timeout_secs),
        network: Some(!args.target_opts.no_network),
        builders: builders(&cfg, &args.target_opts),
        any_worker: args.target_opts.any_worker,
        max_price_usd: args.target_opts.max_price,
        ..Default::default()
    };
    let result = submit(cfg, req, progress.clone()).await;
    for t in temps {
        let _ = std::fs::remove_file(t);
    }
    let outcome = result?;
    summarize(&outcome);
    print_tail_if_failed(&outcome);
    std::fs::create_dir_all(&args.dest)?;
    for (path, file) in &outcome.outputs {
        let status = std::process::Command::new("tar")
            .args([
                "-xzf",
                &file.display().to_string(),
                "-C",
                &args.dest.display().to_string(),
            ])
            .status()
            .context("run tar")?;
        if status.success() {
            eprintln!("output {path} -> {}", args.dest.display());
            let _ = std::fs::remove_file(file);
        } else {
            eprintln!("could not unpack output {path}; kept {}", file.display());
        }
    }
    if let Some(missing) = outcome.result["outputs"].as_array() {
        for m in missing.iter().filter(|m| m["missing"] == true) {
            eprintln!(
                "output {} was not produced",
                m["path"].as_str().unwrap_or("?")
            );
        }
    }
    if args.target_opts.json {
        println!("{}", serde_json::to_string_pretty(&outcome.result)?);
    }
    let code = outcome.result["exit_code"]
        .as_i64()
        .unwrap_or(if outcome.ok() { 0 } else { 1 });
    if code != 0 {
        std::process::exit(code.clamp(1, 255) as i32);
    }
    Ok(())
}

pub fn run_containers(cmd: ContainersCmd, config_path: &Path) -> Result<()> {
    let mut cfg =
        Config::load_raw(config_path).or_else(|_| Config::load_or_default(config_path))?;
    let valid = |p: &str| -> Result<String> {
        let p = p.trim();
        if !c0mpute_core::private::valid_peer_id(p) {
            bail!("{p:?} is not a peer id");
        }
        Ok(p.to_string())
    };
    // Builders may also be full multiaddrs ending in /p2p/<peer id>.
    let valid_builder = |p: &str| -> Result<String> {
        container_run::split_builders(&[p.to_string()])?;
        Ok(p.trim().to_string())
    };
    let changed = match cmd {
        ContainersCmd::Show => {
            let c = &cfg.containers;
            let id = c0mpute_net::identity::existing_peer_id(&container_run::identity_dir());
            println!("work_dir      {}", c.work_dir().display());
            println!("open          {}", c.open);
            println!(
                "buyers        {}",
                if c.buyers.is_empty() {
                    "(this machine only)".into()
                } else {
                    c.buyers.join(", ")
                }
            );
            println!(
                "builders      {}",
                if c.builders.is_empty() {
                    "(none)".into()
                } else {
                    c.builders.join(", ")
                }
            );
            println!(
                "limits        max_jobs {} cpus {} memory {} MiB disk {} GiB timeout {}s cache {} GiB",
                c.max_jobs,
                c.cpu_cap(),
                c.memory_cap_mb(),
                c.max_disk_gb,
                c.max_timeout_secs,
                c.cache_gb
            );
            println!(
                "egress guard  {}{}",
                c.egress_guard,
                c.guard_build_bridge
                    .as_deref()
                    .map(|b| format!(" (+ build bridge {b})"))
                    .unwrap_or_default()
            );
            println!(
                "build id      {}",
                id.map(|p| p.to_base58())
                    .unwrap_or_else(|| "(created on the first `c0mpute build`)".into())
            );
            println!(
                "role          {}",
                if cfg.roles.contains(&c0mpute_proto::Role::Containers) {
                    "on"
                } else {
                    "off (add \"containers\" to roles, or worker start --roles ...,containers)"
                }
            );
            false
        }
        ContainersCmd::Id => {
            let dir = container_run::identity_dir();
            std::fs::create_dir_all(&dir)?;
            let kp = c0mpute_net::identity::load_or_create(&dir)?;
            println!("{}", kp.public().to_peer_id());
            false
        }
        ContainersCmd::Allow { peer_id } => {
            let p = valid(&peer_id)?;
            if cfg.containers.buyers.contains(&p) {
                false
            } else {
                cfg.containers.buyers.push(p);
                true
            }
        }
        ContainersCmd::Deny { peer_id } => {
            let before = cfg.containers.buyers.len();
            cfg.containers.buyers.retain(|b| b != peer_id.trim());
            before != cfg.containers.buyers.len()
        }
        ContainersCmd::Open => !std::mem::replace(&mut cfg.containers.open, true),
        ContainersCmd::Close => std::mem::replace(&mut cfg.containers.open, false),
        ContainersCmd::AddBuilder { peer_id } => {
            let p = valid_builder(&peer_id)?;
            if cfg.containers.builders.contains(&p) {
                false
            } else {
                cfg.containers.builders.push(p);
                true
            }
        }
        ContainersCmd::RmBuilder { peer_id } => {
            let before = cfg.containers.builders.len();
            cfg.containers.builders.retain(|b| b != peer_id.trim());
            before != cfg.containers.builders.len()
        }
    };
    if changed {
        cfg.save(config_path)?;
        println!(
            "saved {} (restart the worker for worker-side changes)",
            config_path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_pairs() {
        let m = kv(&["A=1".into(), "B=x=y".into()], "--env").unwrap();
        assert_eq!(m["A"], "1");
        assert_eq!(m["B"], "x=y");
        assert!(kv(&["nope".into()], "--env").is_err());
    }
}

// ─── MCP (`c0mpute apps mcp`) ────────────────────────────────────────────

fn mcp_target(a: &serde_json::Value) -> Target {
    use serde_json::Value;
    Target {
        peers: a
            .get("peers")
            .and_then(Value::as_array)
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        any_worker: a
            .get("any_worker")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        cpus: a.get("cpus").and_then(Value::as_f64),
        memory_mb: a.get("memory_mb").and_then(Value::as_u64),
        disk_mb: a.get("disk_mb").and_then(Value::as_u64),
        timeout_secs: a
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(1800),
        no_network: a.get("network").and_then(Value::as_bool) == Some(false),
        max_price: a.get("max_price").and_then(Value::as_f64).unwrap_or(0.0),
        json: false,
    }
}

fn mcp_map(a: &serde_json::Value, key: &str) -> BTreeMap<String, String> {
    a.get(key)
        .and_then(serde_json::Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn mcp_progress() -> Progress {
    // stdout is the MCP channel: progress goes to stderr.
    Arc::new(|line: &str| eprintln!("[c0mpute build] {line}"))
}

/// `container_build`: build `dir` on a builder; write the image to `output`
/// and/or load it into the local docker with `tags`.
pub async fn mcp_build(a: &serde_json::Value) -> Result<serde_json::Value> {
    let cfg = Config::load_or_default(&c0mpute_core::config::default_config_path())?;
    let t = mcp_target(a);
    let dir = a
        .get("dir")
        .and_then(serde_json::Value::as_str)
        .context("dir is required")?;
    let progress = mcp_progress();
    let mut temps = Vec::new();
    let context = tarball(Path::new(dir), true, &progress, &mut temps)?;
    let req = Request {
        context: Some(context),
        build: Some(BuildSpec {
            dockerfile: a
                .get("dockerfile")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Dockerfile")
                .to_string(),
            build_args: mcp_map(a, "build_args"),
            target: a
                .get("target")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        }),
        timeout_secs: Some(t.timeout_secs),
        disk_mb: t.disk_mb,
        network: Some(!t.no_network),
        builders: builders(&cfg, &t),
        any_worker: t.any_worker,
        max_price_usd: t.max_price,
        ..Default::default()
    };
    let result = submit(cfg, req, progress).await;
    for f in temps {
        let _ = std::fs::remove_file(f);
    }
    let outcome = result?;
    let mut out = serde_json::json!({ "ok": outcome.ok(), "worker": outcome.worker, "job_id": outcome.job_id, "result": outcome.result });
    let Some(image) = outcome.image.clone() else {
        return Ok(out);
    };
    let id = outcome.result["image"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let tags: Vec<String> = a
        .get("tags")
        .and_then(serde_json::Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if a.get("load")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || !tags.is_empty()
    {
        let loaded = docker(&["load", "--input", &image.display().to_string()])?;
        let source = loaded
            .lines()
            .find_map(|l| l.strip_prefix("Loaded image: "))
            .map(str::to_string)
            .unwrap_or_else(|| id.clone());
        for tag in &tags {
            docker(&["tag", &source, tag])?;
        }
        out["loaded"] = serde_json::json!(id);
    }
    match a.get("output").and_then(serde_json::Value::as_str) {
        Some(dest) => {
            if std::fs::rename(&image, dest).is_err() {
                std::fs::copy(&image, dest)?;
                let _ = std::fs::remove_file(&image);
            }
            out["output"] = serde_json::json!(dest);
        }
        None => {
            let _ = std::fs::remove_file(&image);
        }
    }
    Ok(out)
}

/// `container_run`: run `cmd` in `image` (or a `build` of a dir) with `input`
/// copied in, and unpack `outputs` into `dest`.
pub async fn mcp_run(a: &serde_json::Value) -> Result<serde_json::Value> {
    use serde_json::Value;
    let cfg = Config::load_or_default(&c0mpute_core::config::default_config_path())?;
    let t = mcp_target(a);
    let progress = mcp_progress();
    let mut temps = Vec::new();
    let cmd: Vec<String> = a
        .get("cmd")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if cmd.is_empty() {
        bail!("cmd (an array of strings) is required");
    }
    let build_dir = a.get("build").and_then(Value::as_str);
    let context = build_dir
        .map(|d| tarball(Path::new(d), true, &progress, &mut temps))
        .transpose()?;
    let input = a
        .get("input")
        .and_then(Value::as_str)
        .map(|d| tarball(Path::new(d), false, &progress, &mut temps))
        .transpose()?;
    let req = Request {
        context,
        build: build_dir.map(|_| BuildSpec {
            dockerfile: a
                .get("dockerfile")
                .and_then(Value::as_str)
                .unwrap_or("Dockerfile")
                .to_string(),
            build_args: mcp_map(a, "build_args"),
            target: a.get("target").and_then(Value::as_str).map(str::to_string),
        }),
        image: a.get("image").and_then(Value::as_str).map(str::to_string),
        cmd,
        input,
        env: mcp_map(a, "env"),
        workdir: a.get("workdir").and_then(Value::as_str).map(str::to_string),
        outputs: a
            .get("outputs")
            .and_then(Value::as_array)
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        cpus: t.cpus,
        memory_mb: t.memory_mb,
        disk_mb: t.disk_mb,
        timeout_secs: Some(t.timeout_secs),
        network: Some(!t.no_network),
        builders: builders(&cfg, &t),
        any_worker: t.any_worker,
        max_price_usd: t.max_price,
        ..Default::default()
    };
    let result = submit(cfg, req, progress).await;
    for f in temps {
        let _ = std::fs::remove_file(f);
    }
    let outcome = result?;
    let dest = PathBuf::from(a.get("dest").and_then(Value::as_str).unwrap_or("."));
    std::fs::create_dir_all(&dest)?;
    let mut unpacked = Vec::new();
    for (path, file) in &outcome.outputs {
        let ok = std::process::Command::new("tar")
            .args([
                "-xzf",
                &file.display().to_string(),
                "-C",
                &dest.display().to_string(),
            ])
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            let _ = std::fs::remove_file(file);
        }
        unpacked.push(serde_json::json!({ "path": path, "unpacked": ok, "file": if ok { None } else { Some(file) } }));
    }
    Ok(serde_json::json!({
        "ok": outcome.ok(),
        "worker": outcome.worker,
        "job_id": outcome.job_id,
        "exit_code": outcome.result["exit_code"],
        "log_tail": outcome.result["log_tail"],
        "error": outcome.result["error"],
        "outputs": unpacked,
        "dest": dest,
    }))
}
