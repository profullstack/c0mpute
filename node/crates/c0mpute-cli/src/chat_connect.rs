//! `c0mpute chat connect hosting --name <node>`: link this machine to your
//! c0mpute chat account as a hosting node, then publish what you push from
//! the chat.
//!
//! ```text
//! c0mpute chat login                             # CoinPay sign-in (DID required)
//! c0mpute chat connect hosting --name seed2      # link + serve pushes (leave running)
//! ```
//!
//! In c0mpute.com/chat, `/push hosting <name>` queues the chat's files (images,
//! songs, album zips, promo videos, the transcript) for this node. The node
//! polls c0mpute.com with the account key, downloads the files into
//! `<data_dir>/pushes/<name>/`, and runs
//! `c0mpute hosting publish <dir> --name <name> --disks auto` as a child, so
//! the site is signed by this node's own key and its hosts are paid from this
//! node's CoinPay wallet (`[payments] enabled = true, rail = "coinpay"`).
//! Progress lines stream back to the chat while it runs. `/push hosting rm
//! <name>` runs `c0mpute hosting rm <name>` here.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::ai_chat::{api_error, base_url, client, require_key};

#[derive(Parser, Debug)]
#[command(
    name = "c0mpute chat connect",
    about = "Link this node to your c0mpute chat account and publish what you /push",
    long_about = "Link this node to your c0mpute chat account (sign in first with `c0mpute chat login`),\n\
then publish what you push from c0mpute.com/chat:\n\n  \
c0mpute chat connect hosting --name seed2\n\n\
In the chat: /connect hosting seed2, then /push hosting <site>. Sites are signed by this\n\
node's key and paid hosts (--disks auto) are paid from its CoinPay wallet, so\n\
[payments] must be enabled with rail = \"coinpay\" and the API key in its env."
)]
struct Args {
    /// What to connect: `hosting` (the only target so far).
    #[arg(default_value = "hosting")]
    target: String,
    /// This node's name on your account (1-63 of a-z, 0-9 and '-'). Default: the hostname.
    #[arg(long)]
    name: Option<String>,
    /// Seconds between polls for new pushes.
    #[arg(long, default_value_t = 5)]
    interval: u64,
    /// Handle what is waiting once, then exit.
    #[arg(long)]
    once: bool,
    /// Seconds `hosting publish` waits for network copies.
    #[arg(long, default_value_t = 600)]
    wait_secs: u64,
    /// Where pushed files are kept (default <data_dir>/pushes).
    #[arg(long)]
    dir: Option<PathBuf>,
    /// c0mpute site URL (default https://c0mpute.com, or $C0MPUTE_URL).
    #[arg(long)]
    url: Option<String>,
}

#[derive(Deserialize, Debug)]
struct Job {
    id: String,
    rev: i64,
    action: String,
    name: String,
    #[serde(default)]
    disks: Option<String>,
    #[serde(default)]
    files: Vec<JobFile>,
}

#[derive(Deserialize, Debug)]
struct JobFile {
    path: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

/// Same rule as `hosting publish --name`.
pub(crate) fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// A pushed file's path, kept inside the site folder: relative, no `..`, no
/// hidden segments, at most 4 deep. The server checks this too.
pub(crate) fn safe_rel(p: &str) -> Option<PathBuf> {
    if p.is_empty() || p.len() > 400 || p.contains('\\') || p.contains('\0') {
        return None;
    }
    let path = Path::new(p);
    let mut out = PathBuf::new();
    let mut depth = 0;
    for c in path.components() {
        match c {
            Component::Normal(s) => {
                let s = s.to_str()?;
                if s.starts_with('.') || s.trim() != s {
                    return None;
                }
                out.push(s);
                depth += 1;
            }
            _ => return None,
        }
    }
    (depth > 0 && depth <= 4).then_some(out)
}

/// `XDG_DATA_HOME` for the `hosting publish` / `hosting rm` children: their
/// data dir (and so their publisher identity) is `<root>/.node/c0mpute`, apart
/// from the node's own `buyer` key that manual publishes use.
pub(crate) fn node_data_home(root: &Path) -> PathBuf {
    root.join(".node")
}

/// "coinpay" when this node can pay hosts through CoinPay, else "off".
fn payments_mode() -> String {
    let cfg = c0mpute_core::Config::load_or_default(&c0mpute_core::config::default_config_path()).unwrap_or_default();
    let p = &cfg.payments;
    let key = std::env::var(&p.coinpay.api_key_env).map(|v| !v.trim().is_empty()).unwrap_or(false);
    if p.enabled && p.rail == "coinpay" && key {
        "coinpay".into()
    } else {
        "off".into()
    }
}

fn default_node_name() -> String {
    let raw = std::fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "node".into());
    let mut s: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    s.truncate(63);
    let s = s.trim_matches('-').to_string();
    if valid_name(&s) { s } else { "node".into() }
}

/// The last progress line in a chunk of child stderr: split on CR/LF, ANSI stripped.
pub(crate) fn last_line(buf: &str) -> Option<String> {
    buf.split(['\r', '\n'])
        .map(strip_ansi)
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .last()
}

pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// Pull `site:`, `version:`, `copies:` and `url:` out of `hosting publish` stdout.
pub(crate) fn parse_publish(stdout: &str) -> (Option<String>, Option<u64>, Option<u64>, Option<String>) {
    let mut site = None;
    let mut version = None;
    let mut copies = None;
    let mut url = None;
    for l in stdout.lines() {
        let Some((k, v)) = l.split_once(':') else { continue };
        let v = v.trim();
        match k.trim() {
            "site" => site = v.split_whitespace().next().map(str::to_string),
            "version" => version = v.parse().ok(),
            "copies" => copies = v.split('/').next().and_then(|n| n.trim().parse().ok()),
            "url" if url.is_none() => url = v.split_whitespace().next().map(str::to_string),
            _ => {}
        }
    }
    (site, version, copies, url)
}

struct Api {
    base: String,
    key: String,
    http: reqwest::Client,
    node: String,
    /// XDG_DATA_HOME for the publish/rm children (see [`node_data_home`]).
    data_home: PathBuf,
}

impl Api {
    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let res = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.key)
            .timeout(Duration::from_secs(30))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {path}"))?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error(status, &text));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn jobs(&self) -> Result<Vec<Job>> {
        let res = self
            .http
            .get(format!("{}/api/v1/hosting/nodes/{}/jobs", self.base, self.node))
            .bearer_auth(&self.key)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context("poll for pushes")?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error(status, &text));
        }
        let v: Value = serde_json::from_str(&text)?;
        Ok(serde_json::from_value(v["jobs"].clone()).unwrap_or_default())
    }

    async fn progress(&self, job: &Job, line: &str) {
        let body = json!({"node": self.node, "rev": job.rev, "progress": line});
        if let Err(e) = self.post(&format!("/api/v1/hosting/pushes/{}", job.id), body).await {
            eprintln!("  (progress not sent: {e:#})");
        }
    }

    async fn report(&self, job: &Job, mut body: Value) -> Result<()> {
        body["node"] = json!(self.node);
        body["rev"] = json!(job.rev);
        self.post(&format!("/api/v1/hosting/pushes/{}", job.id), body).await.map(|_| ())
    }

    /// Stream one media file to disk; returns its size.
    async fn download(&self, url: &str, to: &Path) -> Result<u64> {
        let mut res = self
            .http
            .get(format!("{}{url}", self.base))
            .bearer_auth(&self.key)
            .timeout(Duration::from_secs(30 * 60))
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            return Err(api_error(status, &text));
        }
        let tmp = to.with_extension("partial");
        let mut f = tokio::fs::File::create(&tmp).await?;
        let mut n = 0u64;
        while let Some(chunk) = res.chunk().await? {
            n += chunk.len() as u64;
            if n > 4 * 1024 * 1024 * 1024 {
                bail!("file is over 4 GiB");
            }
            f.write_all(&chunk).await?;
        }
        f.flush().await?;
        drop(f);
        tokio::fs::rename(&tmp, to).await?;
        Ok(n)
    }
}

pub async fn run(args: &[String]) -> Result<()> {
    let a = Args::try_parse_from(std::iter::once("c0mpute chat connect".to_string()).chain(args.iter().cloned()))
        .unwrap_or_else(|e| e.exit());
    if a.target != "hosting" {
        bail!("unknown target {:?}: `c0mpute chat connect hosting` is the only one so far", a.target);
    }
    let name = a.name.clone().unwrap_or_else(default_node_name).to_ascii_lowercase();
    if !valid_name(&name) {
        bail!("--name {name:?}: use 1-63 of a-z, 0-9 and '-'");
    }
    let key = require_key()?;
    let base = base_url(a.url.as_deref());
    let root = a
        .dir
        .clone()
        .unwrap_or_else(|| c0mpute_core::config::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("pushes"));
    std::fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;

    // Pushes get their own data dir (identity, scan cache) under <root>/.node:
    // two processes publishing with one libp2p identity fight over the peer id
    // (a manual `hosting publish` on this machine stalled at 0 MB/s that way).
    let node_data = node_data_home(&root);
    let peer_id = c0mpute_net::identity::load_or_create(&node_data.join("c0mpute").join("buyer"))
        .map(|k| k.public().to_peer_id().to_string())
        .context("the push publisher identity")?;
    let payments = payments_mode();
    let mut api = Api { base: base.clone(), key, http: client()?, node: String::new(), data_home: node_data.clone() };
    let reg = api
        .post(
            "/api/v1/hosting/nodes",
            json!({"name": name, "peer_id": peer_id, "payments": payments, "version": env!("CARGO_PKG_VERSION")}),
        )
        .await
        .context("link this node to your account (signed in? `c0mpute chat login`)")?;
    api.node = reg["node"]["id"].as_str().ok_or_else(|| anyhow!("no node id in {reg}"))?.to_string();

    println!("linked:   {name} ({}) on {base}", api.node);
    println!("identity: {peer_id} (signs every site pushed through this node; kept in {})", node_data.display());
    if payments == "coinpay" {
        println!("payments: CoinPay: paid hosts (--disks auto) are paid from this node's wallet");
    } else {
        println!(
            "payments: OFF: pushes to this node are refused until [payments] enabled = true, rail = \"coinpay\" \
             and the CoinPay API key is in its env; then run this again"
        );
    }
    println!("files:    {}", root.display());
    println!("In c0mpute.com/chat: /connect hosting {name}, then /push hosting <site>. Ctrl-C stops.");

    let mut backoff = a.interval.max(2);
    loop {
        match api.jobs().await {
            Ok(jobs) => {
                backoff = a.interval.max(2);
                for job in jobs {
                    let started = Instant::now();
                    let r = match job.action.as_str() {
                        "rm" => rm(&api, &job).await,
                        _ => publish(&api, &job, &root, a.wait_secs).await,
                    };
                    match r {
                        Ok(()) => eprintln!("{} {}: done in {:.0}s", job.action, job.name, started.elapsed().as_secs_f64()),
                        Err(e) => {
                            eprintln!("{} {}: {e:#}", job.action, job.name);
                            let _ = api.report(&job, json!({"ok": false, "error": format!("{e:#}")})).await;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("poll: {e:#} (retrying in {backoff}s)");
                backoff = (backoff * 2).min(300);
            }
        }
        if a.once {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
    }
}

async fn publish(api: &Api, job: &Job, root: &Path, wait_secs: u64) -> Result<()> {
    if !valid_name(&job.name) {
        bail!("bad site name {:?}", job.name);
    }
    eprintln!("push {} (rev {}): {} files", job.name, job.rev, job.files.len());
    // A fresh folder per push: the site is exactly what was pushed this time.
    // `hosting publish` keeps a scan cache, so unchanged files are not rehashed.
    let dir = root.join(&job.name);
    if dir.exists() {
        tokio::fs::remove_dir_all(&dir).await.with_context(|| format!("clear {}", dir.display()))?;
    }
    tokio::fs::create_dir_all(&dir).await?;
    let total = job.files.len();
    let mut bytes = 0u64;
    let mut skipped: Vec<String> = Vec::new();
    let mut last = Instant::now() - Duration::from_secs(10);
    for (i, f) in job.files.iter().enumerate() {
        let rel = safe_rel(&f.path).ok_or_else(|| anyhow!("refusing file path {:?}", f.path))?;
        let to = dir.join(&rel);
        if let Some(parent) = to.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if last.elapsed() >= Duration::from_secs(1) || i + 1 == total {
            api.progress(job, &format!("downloading {}/{total}: {}", i + 1, f.path)).await;
            last = Instant::now();
        }
        if let Some(text) = &f.text {
            tokio::fs::write(&to, text.as_bytes()).await?;
            bytes += text.len() as u64;
        } else if let Some(url) = &f.url {
            if !url.starts_with("/api/v1/hosting/pushes/") {
                bail!("refusing file url {url:?}");
            }
            // Old results expire from the network's media store: publish the
            // rest and say what was left out, rather than failing the push.
            match api.download(url, &to).await {
                Ok(n) => bytes += n,
                Err(e) if e.to_string().starts_with("HTTP 404") || e.to_string().starts_with("HTTP 409") => {
                    eprintln!("  skipped {}: {e:#}", f.path);
                    skipped.push(f.path.clone());
                }
                Err(e) => return Err(e.context(f.path.clone())),
            }
        } else {
            bail!("{}: no text or url", f.path);
        }
    }
    api.progress(job, &format!("{total} files, {:.1} MiB: publishing", bytes as f64 / 1048576.0)).await;

    let disks = job.disks.clone().unwrap_or_else(|| "auto".into());
    let exe = std::env::current_exe().context("own executable")?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(publish_args(&dir, &job.name, wait_secs, &disks));
    let (ok, stdout, tail) = run_streaming(api, job, cmd).await?;
    let (site, version, copies, url) = parse_publish(&stdout);
    match (ok, site) {
        (true, Some(site)) => {
            api.report(
                job,
                json!({
                    "ok": true, "site_id": site, "version": version, "url": url,
                    "copies": copies.unwrap_or(0), "bytes": bytes,
                    "note": (!skipped.is_empty()).then(|| format!("left out (expired on the network): {}", skipped.join(", "))),
                }),
            )
            .await?;
            println!("live:     {} -> {}", job.name, url.unwrap_or_default());
            Ok(())
        }
        _ => bail!("hosting publish failed: {}", tail.unwrap_or_else(|| "no output".into())),
    }
}

async fn rm(api: &Api, job: &Job) -> Result<()> {
    if !valid_name(&job.name) {
        bail!("bad site name {:?}", job.name);
    }
    api.progress(job, "removing the site from the network").await;
    let exe = std::env::current_exe().context("own executable")?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(rm_args(&job.name));
    let (ok, _stdout, tail) = run_streaming(api, job, cmd).await?;
    if !ok {
        bail!("hosting rm failed: {}", tail.unwrap_or_else(|| "no output".into()));
    }
    api.report(job, json!({"ok": true, "deleted": true})).await?;
    println!("removed:  {}", job.name);
    Ok(())
}

/// `c0mpute hosting publish <dir> --name <n> --wait-secs <s> [--disks <q>]`.
pub(crate) fn publish_args(dir: &Path, name: &str, wait_secs: u64, disks: &str) -> Vec<std::ffi::OsString> {
    let mut a: Vec<std::ffi::OsString> = vec![
        "hosting".into(),
        "publish".into(),
        dir.as_os_str().to_owned(),
        "--name".into(),
        name.into(),
        "--wait-secs".into(),
        wait_secs.to_string().into(),
    ];
    if disks != "free" {
        a.push("--disks".into());
        a.push(disks.into());
    }
    a
}

/// `c0mpute hosting rm --name <n>`.
pub(crate) fn rm_args(name: &str) -> Vec<std::ffi::OsString> {
    vec!["hosting".into(), "rm".into(), "--name".into(), name.into()]
}

/// Run a child, sending its latest stderr line to the chat at most every 2 s.
/// Returns (exit ok, stdout, last stderr line).
async fn run_streaming(api: &Api, job: &Job, mut cmd: tokio::process::Command) -> Result<(bool, String, Option<String>)> {
    // Linux: ProjectDirs follows XDG_DATA_HOME, so the child publishes with the
    // push identity; [payments] still comes from the usual config dir.
    cmd.env("XDG_DATA_HOME", &api.data_home).env("C0MPUTE_NO_AUTO_UPDATE", "1");
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().context("start c0mpute hosting")?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let reader = tokio::spawn(async move {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s).await;
        s
    });
    let mut buf = [0u8; 8192];
    let mut pending = String::new();
    let mut last_line_seen: Option<String> = None;
    let mut last_sent = Instant::now() - Duration::from_secs(10);
    let mut sent: Option<String> = None;
    let mut first_error: Option<String> = None;
    loop {
        let n = err.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        let chunk = String::from_utf8_lossy(&buf[..n]);
        eprint!("{chunk}");
        pending.push_str(&chunk);
        if first_error.is_none() {
            first_error = pending
                .split(['\r', '\n'])
                .map(strip_ansi)
                .map(|l| l.trim().to_string())
                .find(|l| l.starts_with("error") || l.starts_with("Error"));
        }
        if let Some(l) = last_line(&pending) {
            last_line_seen = Some(l);
        }
        // Keep only the unfinished tail of the buffer.
        if let Some(i) = pending.rfind(['\r', '\n']) {
            pending = pending[i + 1..].to_string();
        }
        if last_sent.elapsed() >= Duration::from_secs(2) && last_line_seen != sent {
            if let Some(l) = &last_line_seen {
                api.progress(job, l).await;
            }
            sent = last_line_seen.clone();
            last_sent = Instant::now();
        }
    }
    let status = child.wait().await?;
    let stdout = reader.await.unwrap_or_default();
    // On failure the first `error:` line says why; clap's last line is only "try --help".
    let tail = if status.success() { last_line_seen } else { first_error.or(last_line_seen) };
    Ok((status.success(), stdout, tail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_hosting_publish() {
        assert!(valid_name("seed2"));
        assert!(valid_name("my-site-01"));
        assert!(!valid_name("-x"));
        assert!(!valid_name("Seed2"));
        assert!(!valid_name("a_b"));
        assert!(!valid_name(""));
        assert!(!valid_name(&"a".repeat(64)));
    }

    #[test]
    fn pushed_paths_stay_inside_the_site() {
        assert_eq!(safe_rel("music/01 - Song.mp3"), Some(PathBuf::from("music/01 - Song.mp3")));
        for bad in ["../x", "a/../b", "/etc/passwd", ".env", "a/.git/x", "a\\b", "", "a/b/c/d/e", " a"] {
            assert_eq!(safe_rel(bad), None, "{bad}");
        }
    }

    #[test]
    fn progress_lines_from_a_tty_style_stream() {
        assert_eq!(last_line("  hashing 10%\r\x1b[2K  hashing 50%\r").as_deref(), Some("hashing 50%"));
        assert_eq!(last_line("a\nb\n").as_deref(), Some("b"));
        assert_eq!(last_line("\r\n"), None);
    }

    /// The child command lines must parse with the real CLI (a bare `hosting
    /// rm <name>` shipped once and failed every delete).
    #[test]
    fn child_commands_parse_with_the_real_cli() {
        use clap::Parser;
        let parse = |args: Vec<std::ffi::OsString>| {
            crate::Cli::try_parse_from(std::iter::once(std::ffi::OsString::from("c0mpute")).chain(args))
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        parse(publish_args(Path::new("/tmp/site"), "seed2", 600, "auto")).unwrap();
        parse(publish_args(Path::new("/tmp/site"), "seed2", 600, "free")).unwrap();
        parse(rm_args("seed2")).unwrap();
    }

    #[test]
    fn reads_publish_output() {
        let out = "site:    ec601e419ec508ad0d9b\nversion: 1762800000000\ncopies:  3/3\npaid:    3 preferred host(s)\nurl:     https://gateway.c0mpute.com/s/ec601e419ec508ad0d9b/\npath:    /s/ec601e419ec508ad0d9b/   (works)\n";
        let (site, version, copies, url) = parse_publish(out);
        assert_eq!(site.as_deref(), Some("ec601e419ec508ad0d9b"));
        assert_eq!(version, Some(1762800000000));
        assert_eq!(copies, Some(3));
        assert_eq!(url.as_deref(), Some("https://gateway.c0mpute.com/s/ec601e419ec508ad0d9b/"));
    }
}
