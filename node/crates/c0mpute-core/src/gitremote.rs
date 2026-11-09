//! `git-remote-c0mpute`: git repositories kept by the network.
//!
//! ```text
//! git remote add c0 c0mpute://my-api        # a repo this machine owns
//! git push c0 main                          # store it (and deploy it)
//! git clone c0mpute://<repo_id>             # anyone, owner online or not
//! ```
//!
//! A repository is a hosting site named `git-<name>`, signed by the owner's
//! key, holding two files: `refs.json` (ref → commit, and HEAD) and
//! `repo.bundle` (a full `git bundle` of every ref). The network keeps three
//! copies like any site, so a clone works with the owner offline; the record
//! signature means only the owner can move a ref. Pushes are fast-forward
//! only unless forced (`+ref`), checked here against the published refs.
//!
//! When a push updates HEAD's branch and that commit has a `c0mpute.toml`
//! with an `[app]` table, the helper deploys it as an app
//! ([`crate::apps::deploy`]). `git config c0mpute.deploy false` turns that off.
//!
//! git speaks the remote-helper protocol on stdin/stdout, so everything
//! human goes to stderr.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::hosting::{self, Hosting, site};

pub const SITE_PREFIX: &str = "git-";
pub const REFS_FILE: &str = "refs.json";
pub const BUNDLE_FILE: &str = "repo.bundle";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refs {
    /// The ref HEAD points at (`refs/heads/main`).
    #[serde(default)]
    pub head: Option<String>,
    pub refs: BTreeMap<String, String>,
}

/// What a `c0mpute://` URL names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A repo of this machine's key, by name (pushable).
    Own(String),
    /// Anyone's repo, by site id (read-only unless it is ours).
    Id(String),
}

pub fn parse_url(url: &str) -> Result<Target> {
    let rest = url
        .strip_prefix("c0mpute://")
        .or_else(|| url.strip_prefix("c0mpute:"))
        .unwrap_or(url)
        .trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    if crate::apps::record::valid_app_id(rest) {
        return Ok(Target::Id(rest.to_string()));
    }
    let name = rest.strip_prefix(SITE_PREFIX).unwrap_or(rest);
    if !site::valid_name(name) || name.len() + SITE_PREFIX.len() > site::MAX_NAME_CHARS {
        bail!("c0mpute repo URLs are c0mpute://<name> (yours) or c0mpute://<20-hex repo id>");
    }
    Ok(Target::Own(name.to_string()))
}

/// The site id a repo has.
pub fn site_id(target: &Target) -> Result<String> {
    match target {
        Target::Id(id) => Ok(id.clone()),
        Target::Own(name) => {
            let kp = hosting::publisher_keypair()?;
            let owner = site::owner_hex(&kp).context("publisher key is not ed25519")?;
            Ok(site::site_id_for(&owner, &format!("{SITE_PREFIX}{name}")))
        }
    }
}

fn git(git_dir: &Path) -> std::process::Command {
    let mut c = std::process::Command::new("git");
    c.env("GIT_DIR", git_dir).stdin(std::process::Stdio::null());
    c
}

fn git_out(git_dir: &Path, args: &[&str]) -> Result<String> {
    let out = git(git_dir)
        .args(args)
        .stderr(std::process::Stdio::piped())
        .output()
        .context("run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn is_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_ref(r: &str) -> bool {
    r.starts_with("refs/")
        && r.len() < 256
        && !r.contains("..")
        && !r.contains(['\n', ' ', '\0', '~', '^', ':'])
}

struct Remote {
    target: Target,
    site_id: String,
    git_dir: PathBuf,
    node: Option<(crate::Supervisor, Hosting)>,
    current: Option<(site::SiteRecord, Refs)>,
    looked: bool,
}

impl Remote {
    async fn hosting(&mut self) -> Result<Hosting> {
        if self.node.is_none() {
            let sup = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
            let h = Hosting::start(
                sup.libp2p.clone(),
                sup.store.clone(),
                hosting::Options::default(),
            )
            .await?;
            self.node = Some((sup, h));
        }
        Ok(self
            .node
            .as_ref()
            .map(|(_, h)| h.clone())
            .expect("just set"))
    }

    /// The published refs, or None for a repo nobody has pushed yet.
    async fn refs(&mut self) -> Result<Option<Refs>> {
        if self.looked {
            return Ok(self.current.as_ref().map(|(_, r)| r.clone()));
        }
        let h = self.hosting().await?;
        let started = Instant::now();
        // A new client hears records by gossip or asks peers; give the swarm
        // a little while to connect.
        let wait = if matches!(self.target, Target::Id(_)) {
            60
        } else {
            25
        };
        let rec = loop {
            if let Some(r) = h.lookup_record(&self.site_id).await {
                break Some(r);
            }
            if started.elapsed() > Duration::from_secs(wait) {
                break None;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        };
        self.looked = true;
        let Some(rec) = rec else {
            if matches!(self.target, Target::Id(_)) {
                bail!("repository {} was not found on the network", self.site_id);
            }
            return Ok(None);
        };
        let (_, bytes) = h
            .read_file(&rec, REFS_FILE)
            .await?
            .context("repo has no refs.json")?;
        let refs: Refs = serde_json::from_slice(&bytes).context("refs.json")?;
        self.current = Some((rec, refs.clone()));
        Ok(Some(refs))
    }

    /// Download the bundle and unpack its objects into the local repo.
    async fn unbundle(&mut self) -> Result<()> {
        let Some((rec, _)) = self.current.clone() else {
            return Ok(());
        };
        let h = self.hosting().await?;
        let (_, bytes) = h
            .read_file(&rec, BUNDLE_FILE)
            .await?
            .context("repo has no bundle")?;
        let tmp = tempfile_path("fetch.bundle");
        std::fs::write(&tmp, &bytes)?;
        let res = git_out(
            &self.git_dir,
            &["bundle", "unbundle", &tmp.display().to_string()],
        );
        let _ = std::fs::remove_file(&tmp);
        res.map(|_| ())
    }

    async fn list(&mut self, out: &mut Vec<String>) -> Result<()> {
        if let Some(refs) = self.refs().await? {
            for (r, sha) in &refs.refs {
                out.push(format!("{sha} {r}"));
            }
            if let Some(head) = refs.head.filter(|h| refs.refs.contains_key(h)) {
                out.push(format!("@{head} HEAD"));
            }
        }
        Ok(())
    }

    async fn push(&mut self, specs: &[String]) -> Result<Vec<String>> {
        let Target::Own(name) = self.target.clone() else {
            return Ok(specs
                .iter()
                .map(|s| {
                    format!(
                        "error {} only the owner can push to this repository",
                        s.rsplit(':').next().unwrap_or("")
                    )
                })
                .collect());
        };
        let old = self.refs().await?.unwrap_or_default();
        if !old.refs.is_empty() {
            self.unbundle().await?;
        }
        let mut new = old.clone();
        let mut replies = Vec::new();
        let mut moved_head = None;
        for spec in specs {
            let (force, spec) = match spec.strip_prefix('+') {
                Some(s) => (true, s),
                None => (false, spec.as_str()),
            };
            let Some((src, dst)) = spec.split_once(':') else {
                replies.push(format!("error {spec} bad refspec"));
                continue;
            };
            if !valid_ref(dst) {
                replies.push(format!("error {dst} bad ref name"));
                continue;
            }
            if src.is_empty() {
                new.refs.remove(dst);
                replies.push(format!("ok {dst}"));
                continue;
            }
            let sha = match git_out(
                &self.git_dir,
                &["rev-parse", "--verify", &format!("{src}^{{commit}}")],
            ) {
                Ok(s) if is_sha(&s) => s,
                _ => {
                    replies.push(format!("error {dst} cannot resolve {src}"));
                    continue;
                }
            };
            if let Some(prev) = old.refs.get(dst)
                && *prev != sha
                && !force
                && git(&self.git_dir)
                    .args(["merge-base", "--is-ancestor", prev, &sha])
                    .status()
                    .map(|s| !s.success())
                    .unwrap_or(true)
            {
                replies.push(format!("error {dst} non-fast-forward"));
                continue;
            }
            new.refs.insert(dst.to_string(), sha.clone());
            replies.push(format!("ok {dst}"));
            if new.head.is_none() && dst.starts_with("refs/heads/") {
                new.head = Some(dst.to_string());
            }
            if new.head.as_deref() == Some(dst) {
                moved_head = Some(sha);
            }
        }
        if new == old {
            return Ok(replies);
        }
        if new
            .head
            .as_ref()
            .map(|h| !new.refs.contains_key(h))
            .unwrap_or(false)
        {
            new.head = new
                .refs
                .keys()
                .find(|k| k.starts_with("refs/heads/"))
                .cloned();
        }
        self.publish(&name, &new).await?;
        if let Some(sha) = moved_head
            && deploy_enabled(&self.git_dir)
        {
            if let Err(e) = deploy_commit(&self.git_dir, &sha).await {
                eprintln!("c0mpute: deploy failed: {e:#}");
            }
        }
        Ok(replies)
    }

    async fn publish(&mut self, name: &str, refs: &Refs) -> Result<()> {
        let dir = tempfile_path("push");
        std::fs::create_dir_all(&dir)?;
        let bundle = dir.join(BUNDLE_FILE);
        // Bundle every published commit under temporary ref names.
        let mut tmp_refs = Vec::new();
        for (i, sha) in refs.refs.values().enumerate() {
            let r = format!("refs/c0mpute-push/{i}");
            git_out(&self.git_dir, &["update-ref", &r, sha])?;
            tmp_refs.push(r);
        }
        let mut args = vec![
            "bundle".to_string(),
            "create".into(),
            bundle.display().to_string(),
        ];
        args.extend(tmp_refs.iter().cloned());
        let made = if tmp_refs.is_empty() {
            std::fs::write(&bundle, b"")?;
            Ok(String::new())
        } else {
            git_out(
                &self.git_dir,
                &args.iter().map(String::as_str).collect::<Vec<_>>(),
            )
        };
        for r in &tmp_refs {
            let _ = git_out(&self.git_dir, &["update-ref", "-d", r]);
        }
        made?;
        std::fs::write(dir.join(REFS_FILE), serde_json::to_vec_pretty(refs)?)?;
        let site_name = format!("{SITE_PREFIX}{name}");
        let built = site::build_from_dir(&dir, &site_name)?;
        let _ = std::fs::remove_dir_all(&dir);
        let keypair = hosting::publisher_keypair()?;
        let manifest = built.manifest_hash();
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let rec = site::SiteRecord::sign(
            &keypair,
            &site_name,
            &manifest,
            seq,
            built.manifest.total_bytes(),
            vec![],
        )?;
        let h = self.hosting().await?;
        h.publish_site(&built, rec.clone()).await?;
        eprintln!(
            "c0mpute: pushed {} ({} bytes), repo id {}",
            site_name, rec.size, rec.site_id
        );
        // Stay until the network holds a copy, so the push survives this
        // machine going away.
        let started = Instant::now();
        let mut copies = 0;
        while started.elapsed() < Duration::from_secs(180) {
            tokio::time::sleep(Duration::from_secs(5)).await;
            h.regossip(&rec).await;
            let n = h.holders(&rec.site_id).await.len();
            if n != copies {
                copies = n;
                eprintln!(
                    "c0mpute:   {copies}/{} network copies",
                    hosting::TARGET_REPLICAS
                );
            }
            if copies >= hosting::TARGET_REPLICAS
                || (copies >= 1 && started.elapsed() > Duration::from_secs(60))
            {
                break;
            }
        }
        if copies == 0 {
            eprintln!(
                "c0mpute: warning: no hosting node took a copy yet; keep this machine online or push again"
            );
        }
        eprintln!("c0mpute: clone with: git clone c0mpute://{}", rec.site_id);
        self.current = Some((rec, refs.clone()));
        Ok(())
    }
}

fn tempfile_path(what: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "c0mpute-git-{}-{}-{what}",
        std::process::id(),
        rand::random::<u32>()
    ))
}

fn deploy_enabled(git_dir: &Path) -> bool {
    git_out(git_dir, &["config", "--bool", "c0mpute.deploy"])
        .map(|v| v != "false")
        .unwrap_or(true)
}

/// Deploy the commit as an app when its tree has `c0mpute.toml` `[app]`.
async fn deploy_commit(git_dir: &Path, sha: &str) -> Result<()> {
    let toml = match git_out(git_dir, &["show", &format!("{sha}:c0mpute.toml")]) {
        Ok(t) => t,
        Err(_) => return Ok(()), // not an app
    };
    let m: crate::apps::deploy::Manifest = toml::from_str(&toml).context("c0mpute.toml")?;
    let Some(app) = m.app else { return Ok(()) };
    let dir = tempfile_path("deploy");
    std::fs::create_dir_all(&dir)?;
    let tar = dir.with_extension("tar");
    git_out(
        git_dir,
        &[
            "archive",
            "--format=tar",
            "-o",
            &tar.display().to_string(),
            sha,
        ],
    )?;
    let st = std::process::Command::new("tar")
        .args([
            "-xf",
            &tar.display().to_string(),
            "-C",
            &dir.display().to_string(),
        ])
        .status()?;
    let _ = std::fs::remove_file(&tar);
    if !st.success() {
        bail!("could not unpack the commit");
    }
    eprintln!(
        "c0mpute: deploying app {} from {}",
        app.name,
        &sha[..12.min(sha.len())]
    );
    let res = crate::apps::deploy::deploy(app.options(&dir, Duration::from_secs(600))).await;
    let _ = std::fs::remove_dir_all(&dir);
    let rec = res?;
    eprintln!(
        "c0mpute: app {} deployed: /a/{}/ on any gateway",
        rec.name, rec.app_id
    );
    Ok(())
}

/// Run the remote-helper protocol (git calls `git-remote-c0mpute <remote> <url>`).
pub async fn remote_helper(_remote: &str, url: &str) -> Result<()> {
    let git_dir = PathBuf::from(std::env::var("GIT_DIR").unwrap_or_else(|_| ".git".into()));
    let target = parse_url(url)?;
    let site_id = site_id(&target)?;
    let mut remote = Remote {
        target,
        site_id,
        git_dir,
        node: None,
        current: None,
        looked: false,
    };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut pending_fetch = false;
    let mut pushes: Vec<String> = Vec::new();
    while let Some(line) = lines.next_line().await? {
        let mut out: Vec<String> = Vec::new();
        let line = line.trim_end().to_string();
        if line == "capabilities" {
            out.extend([
                "fetch".into(),
                "push".into(),
                "option".into(),
                String::new(),
            ]);
        } else if line == "list" || line == "list for-push" {
            remote.list(&mut out).await?;
            out.push(String::new());
        } else if let Some(rest) = line.strip_prefix("option ") {
            let _ = rest;
            out.push("unsupported".into());
        } else if line.starts_with("fetch ") {
            pending_fetch = true;
            continue;
        } else if let Some(spec) = line.strip_prefix("push ") {
            pushes.push(spec.to_string());
            continue;
        } else if line.is_empty() {
            if pending_fetch {
                pending_fetch = false;
                remote.refs().await?;
                remote.unbundle().await?;
                out.push(String::new());
            } else if !pushes.is_empty() {
                let specs = std::mem::take(&mut pushes);
                out.extend(remote.push(&specs).await?);
                out.push(String::new());
            } else {
                break;
            }
        } else {
            bail!("unknown remote-helper command {line:?}");
        }
        for l in out {
            stdout.write_all(l.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
        }
        stdout.flush().await?;
    }
    Ok(())
}

/// `c0mpute git install`: put `git-remote-c0mpute` next to this binary (or in
/// `dir`), as a symlink to it.
pub fn install(dir: Option<&Path>) -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = match dir {
        Some(d) => d.to_path_buf(),
        None => exe
            .parent()
            .context("binary has no directory")?
            .to_path_buf(),
    };
    let link = dir.join("git-remote-c0mpute");
    let _ = std::fs::remove_file(&link);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&exe, &link)
        .with_context(|| format!("create {}", link.display()))?;
    #[cfg(not(unix))]
    std::fs::copy(&exe, &link)?;
    Ok(link)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(
            parse_url("c0mpute://my-api").unwrap(),
            Target::Own("my-api".into())
        );
        assert_eq!(
            parse_url("c0mpute://git-my-api.git").unwrap(),
            Target::Own("my-api".into())
        );
        assert_eq!(
            parse_url("c0mpute://0123456789abcdef0123").unwrap(),
            Target::Id("0123456789abcdef0123".into())
        );
        assert_eq!(parse_url("my-api").unwrap(), Target::Own("my-api".into()));
        assert!(parse_url("c0mpute://My_Api").is_err());
        assert!(parse_url("c0mpute://../etc").is_err());
    }

    #[test]
    fn refs_round_trip_and_validation() {
        let r = Refs {
            head: Some("refs/heads/main".into()),
            refs: BTreeMap::from([("refs/heads/main".into(), "a".repeat(40))]),
        };
        let back: Refs = serde_json::from_slice(&serde_json::to_vec(&r).unwrap()).unwrap();
        assert_eq!(r, back);
        assert!(valid_ref("refs/heads/main") && valid_ref("refs/tags/v1.0"));
        assert!(
            !valid_ref("HEAD") && !valid_ref("refs/heads/../x") && !valid_ref("refs/heads/a b")
        );
        assert!(is_sha(&"f".repeat(40)) && !is_sha("xyz"));
    }

    /// A bundle made the way `publish` makes one restores every commit into a
    /// fresh repository (exercises the git side without the network).
    #[test]
    fn bundle_round_trip() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let base = tempfile_path("t");
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let run = |dir: &Path, args: &[&str]| {
            let o = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.org")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.org")
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        run(&src, &["init", "-q", "-b", "main"]);
        std::fs::write(src.join("a.txt"), "hello").unwrap();
        run(&src, &["add", "."]);
        run(&src, &["commit", "-q", "-m", "one"]);
        let sha = run(&src, &["rev-parse", "HEAD"]);
        let gd = src.join(".git");
        git_out(&gd, &["update-ref", "refs/c0mpute-push/0", &sha]).unwrap();
        let bundle = base.join("r.bundle");
        git_out(
            &gd,
            &[
                "bundle",
                "create",
                &bundle.display().to_string(),
                "refs/c0mpute-push/0",
            ],
        )
        .unwrap();
        let dst = base.join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        run(&dst, &["init", "-q"]);
        git_out(
            &dst.join(".git"),
            &["bundle", "unbundle", &bundle.display().to_string()],
        )
        .unwrap();
        assert_eq!(
            git_out(&dst.join(".git"), &["cat-file", "-t", &sha]).unwrap(),
            "commit"
        );
        let _ = std::fs::remove_dir_all(base);
    }
}
