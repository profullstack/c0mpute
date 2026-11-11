//! `c0mpute chat push hosting ...`: the CLI side of `/push hosting` in
//! c0mpute.com/chat, on the same per-account pushes.
//!
//! ```text
//! c0mpute chat push hosting ls                  # your sites
//! c0mpute chat push hosting nodes               # your linked hosting nodes
//! c0mpute chat push hosting status <name> [-w]  # status + live progress (-w follows it)
//! c0mpute chat push hosting rm <name> [-w]      # take a site down
//! c0mpute chat push hosting <name> <dir> [-w] [--node n]   # push a folder of text files
//! ```
//!
//! Pushing goes through a linked node (`c0mpute chat connect hosting`), which
//! signs the site and pays its hosts through CoinPay. A folder pushed this way
//! travels inline, so it is text only (1 MiB in all); for anything bigger, on
//! a machine with the files, run `c0mpute hosting publish <dir> --name <n>`.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::ai_chat::{api_error, base_url, client, require_key};
use crate::chat_connect::{safe_rel, valid_name};

const USAGE: &str = "usage: c0mpute chat push hosting ls | nodes | status <name> [-w] | rm <name> [-w] | <name> <dir> [-w] [--node <node>]";
const MAX_INLINE: u64 = 1024 * 1024;
const FINAL: [&str; 2] = ["live", "failed"];

struct Http {
    base: String,
    key: String,
    http: reqwest::Client,
}

impl Http {
    async fn send(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<(u16, Value)> {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(&self.key).timeout(Duration::from_secs(30));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() && status.as_u16() != 404 {
            return Err(api_error(status, &text));
        }
        Ok((status.as_u16(), serde_json::from_str(&text).unwrap_or(Value::Null)))
    }
}

pub async fn run(args: &[String]) -> Result<()> {
    let mut words: Vec<&str> = Vec::new();
    let mut watch = false;
    let mut node: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-w" | "--watch" => watch = true,
            "--node" => node = Some(it.next().ok_or_else(|| anyhow!("--node needs a name"))?.clone()),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            w => words.push(w),
        }
    }
    if words.first() == Some(&"hosting") {
        words.remove(0);
    }
    let h = Http { base: base_url(None), key: require_key()?, http: client()? };
    match words.as_slice() {
        ["ls"] | ["list"] => ls(&h).await,
        ["nodes"] => nodes(&h).await,
        ["status", name] => status(&h, name, watch).await,
        ["rm", name] | ["delete", name] => rm(&h, name, watch).await,
        [name, dir] => push(&h, name, Path::new(dir), node, watch).await,
        _ => bail!("{USAGE}"),
    }
}

async fn ls(h: &Http) -> Result<()> {
    let (_, v) = h.send(reqwest::Method::GET, "/api/v1/hosting/pushes", None).await?;
    let pushes = v["pushes"].as_array().cloned().unwrap_or_default();
    if pushes.is_empty() {
        println!("no sites yet: /push hosting <name> in c0mpute.com/chat, or c0mpute chat push hosting <name> <dir>");
    }
    for p in pushes {
        println!(
            "{:<24} {:<10} {:>3} files  {:<12} {}",
            p["name"].as_str().unwrap_or("?"),
            p["status"].as_str().unwrap_or("?"),
            p["files"].as_array().map(Vec::len).unwrap_or(0),
            p["node"].as_str().unwrap_or("?"),
            p["url"].as_str().or(p["progress"].as_str()).unwrap_or("")
        );
        if let Some(e) = p["error"].as_str() {
            println!("{:<24} ! {e}", "");
        }
    }
    Ok(())
}

async fn nodes(h: &Http) -> Result<()> {
    let (_, v) = h.send(reqwest::Method::GET, "/api/v1/hosting/nodes", None).await?;
    let nodes = v["nodes"].as_array().cloned().unwrap_or_default();
    if nodes.is_empty() {
        println!("no hosting node linked: run `c0mpute chat connect hosting --name <node>` on the machine that should publish");
    }
    for n in nodes {
        println!(
            "{:<20} {:<8} {:<26} {}",
            n["name"].as_str().unwrap_or("?"),
            if n["online"].as_bool() == Some(true) { "online" } else { "offline" },
            if n["payments"].as_str() == Some("coinpay") { "pays via CoinPay" } else { "NO CoinPay payments" },
            n["version"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

fn line(p: &Value) -> String {
    let status = p["status"].as_str().unwrap_or("?");
    match status {
        "live" => format!(
            "live: {}{}",
            p["url"].as_str().unwrap_or(""),
            p["copies"].as_u64().map(|c| format!(" ({c} copies)")).unwrap_or_default()
        ),
        "failed" => format!("failed: {}", p["error"].as_str().unwrap_or("?")),
        _ => format!("{status}: {}", p["progress"].as_str().unwrap_or("waiting for the node")),
    }
}

/// Print the push's state; with `watch`, follow it (a spinner line on a TTY) until it settles.
async fn follow(h: &Http, name: &str, watch: bool, removing: bool) -> Result<()> {
    const SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let tty = std::io::stderr().is_terminal();
    let mut i = 0usize;
    let mut last = String::new();
    loop {
        let (code, v) = h.send(reqwest::Method::GET, &format!("/api/v1/hosting/pushes/{name}"), None).await?;
        if code == 404 {
            if tty {
                eprint!("\r\x1b[2K");
            }
            println!("{name}: {}", if removing { "removed from the network" } else { "no such push on this account" });
            return Ok(());
        }
        let p = &v["push"];
        let l = line(p);
        let status = p["status"].as_str().unwrap_or("");
        let done = FINAL.contains(&status) && !(removing && status == "deleting");
        if done || !watch {
            if tty {
                eprint!("\r\x1b[2K");
            }
            println!("{name}: {l}");
            if let Some(e) = p["error"].as_str().filter(|_| status == "live") {
                println!("note: {e}");
            }
            return if status == "failed" { Err(anyhow!("push failed")) } else { Ok(()) };
        }
        if tty {
            eprint!("\r\x1b[2K{} {name}: {l}", SPIN[i % SPIN.len()]);
            let _ = std::io::stderr().flush();
        } else if l != last {
            eprintln!("{name}: {l}");
        }
        last = l;
        i += 1;
        tokio::time::sleep(Duration::from_millis(if tty { 500 } else { 2000 })).await;
    }
}

async fn status(h: &Http, name: &str, watch: bool) -> Result<()> {
    check_name(name)?;
    follow(h, name, watch, false).await
}

async fn rm(h: &Http, name: &str, watch: bool) -> Result<()> {
    check_name(name)?;
    let (code, v) = h.send(reqwest::Method::DELETE, &format!("/api/v1/hosting/pushes/{name}"), None).await?;
    if code == 404 {
        bail!("no push named {name} on this account");
    }
    if v["status"].as_str() == Some("deleted") {
        println!("{name}: removed (it never reached the network)");
        return Ok(());
    }
    println!("{name}: your node is taking it down");
    if watch { follow(h, name, true, true).await } else { Ok(()) }
}

fn check_name(name: &str) -> Result<()> {
    if valid_name(name) { Ok(()) } else { bail!("{name:?} is not a site name: 1-63 of a-z, 0-9 and '-'") }
}

/// A folder's files as inline text files (refuses binary and > 1 MiB in all).
pub(crate) fn inline_files(dir: &Path) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&d)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let rel = p.strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
            if rel.split('/').any(|s| s.starts_with('.')) {
                continue; // hidden files stay home, like hosting publish
            }
            let ft = e.file_type()?;
            if ft.is_symlink() {
                continue; // a link could point anywhere on this machine
            }
            let name = e.file_name().to_string_lossy().to_string();
            if c0mpute_core::hosting::site::never_share_name(&name, ft.is_dir())
                || (ft.is_file() && c0mpute_core::hosting::site::holds_private_key(&p))
            {
                eprintln!("skipping {rel}: key files and node_modules are never pushed");
                continue;
            }
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            if safe_rel(&rel).is_none() {
                bail!("{rel}: path too deep or not allowed in a site");
            }
            total += e.metadata()?.len();
            if total > MAX_INLINE {
                bail!("{} is over 1 MiB: run `c0mpute hosting publish {} --name <name>` on a machine with the files instead", dir.display(), dir.display());
            }
            let text = String::from_utf8(std::fs::read(&p)?)
                .map_err(|_| anyhow!("{rel} is binary: only text files can be pushed from the CLI (use `c0mpute hosting publish` for media)"))?;
            out.push(json!({"path": rel, "type": mime(&rel), "text": text}));
        }
    }
    if out.is_empty() {
        bail!("{} has no files", dir.display());
    }
    Ok(out)
}

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "md" => "text/markdown",
        "svg" => "image/svg+xml",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "vtt" => "text/vtt",
        _ => "text/plain",
    }
}

async fn push(h: &Http, name: &str, dir: &Path, node: Option<String>, watch: bool) -> Result<()> {
    check_name(name)?;
    let files = inline_files(dir)?;
    let n = files.len();
    let mut body = json!({"name": name, "files": files});
    if let Some(node) = node {
        body["node"] = json!(node);
    }
    let (_, v) = h.send(reqwest::Method::POST, "/api/v1/hosting/pushes", Some(body)).await?;
    let p = &v["push"];
    println!(
        "{name}: queued {n} files on node {} (rev {})",
        p["node"].as_str().unwrap_or("?"),
        p["rev"].as_u64().unwrap_or(0)
    );
    if watch { follow(h, name, true, false).await } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_go_inline_as_text_only() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("index.html"), "<h1>hi</h1>").unwrap();
        std::fs::create_dir(d.path().join("css")).unwrap();
        std::fs::write(d.path().join("css/site.css"), "body{}").unwrap();
        std::fs::write(d.path().join(".env"), "SECRET=1").unwrap();
        std::fs::write(d.path().join("id_rsa"), "-----BEGIN RSA PRIVATE KEY-----").unwrap();
        std::fs::write(d.path().join("tls.key"), "-----BEGIN PRIVATE KEY-----").unwrap();
        std::fs::create_dir(d.path().join("node_modules")).unwrap();
        std::fs::write(d.path().join("node_modules/x.js"), "x").unwrap();
        let files = inline_files(d.path()).unwrap();
        let paths: Vec<_> = files.iter().map(|f| f["path"].as_str().unwrap().to_string()).collect();
        assert_eq!(paths, ["index.html", "css/site.css"]);
        assert_eq!(files[0]["type"], "text/html");

        std::fs::write(d.path().join("img.png"), [0x89u8, 0x50, 0xff, 0xfe]).unwrap();
        assert!(inline_files(d.path()).unwrap_err().to_string().contains("binary"));
    }

    #[test]
    fn status_lines() {
        assert_eq!(line(&json!({"status": "live", "url": "https://g/s/x/", "copies": 3})), "live: https://g/s/x/ (3 copies)");
        assert_eq!(line(&json!({"status": "publishing", "progress": "2/3 network copies"})), "publishing: 2/3 network copies");
        assert_eq!(line(&json!({"status": "queued"})), "queued: waiting for the node");
    }
}
