//! `c0mpute git init|clone|push|ls|key|install-helper`, and the
//! `git-remote-c0mpute` entry point (this same binary, run under that name).

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use c0mpute_core::git::{self, client, helper};
use clap::{Parser, Subcommand};

const HELPER: &str = "git-remote-c0mpute";

#[derive(Parser, Debug)]
#[command(
    name = "c0mpute git",
    about = "Git repos kept by the network: signed by your key, three self-healing copies, clone from any gateway"
)]
struct GitCli {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(Subcommand, Debug)]
enum Sub {
    /// Print the c0mpute:// URL of a repo under this machine's key and, inside
    /// a git work tree, add it as a remote. Nothing is published until you push.
    Init {
        repo: String,
        /// Remote name to add (skipped if it already exists).
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// `git clone c0mpute://<owner>/<repo>` with the remote helper on PATH.
    Clone {
        url: String,
        dir: Option<String>,
        /// Gateway(s) to read from (comma-separated; default C0MPUTE_GIT_GATEWAY,
        /// git config c0mpute.gateway, else http://127.0.0.1:7777).
        #[arg(long)]
        gateway: Option<String>,
    },
    /// `git push` with the remote helper on PATH (arguments go to git).
    Push {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        #[arg(long)]
        gateway: Option<String>,
    },
    /// Repos a gateway knows, with live copy counts.
    Ls {
        #[arg(long, default_value = git::DEFAULT_GATEWAY)]
        gateway: String,
    },
    /// This machine's owner key (the <owner> in c0mpute://<owner>/<repo>).
    Key,
    /// Put `git-remote-c0mpute` (a link to this binary) in a directory on
    /// PATH, so plain `git clone/push c0mpute://...` works.
    InstallHelper {
        /// Default: the directory this binary is in.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// The remote helper itself (git runs this; you do not).
    #[command(hide = true)]
    RemoteHelper { remote: String, url: String },
}

/// When this binary runs as `git-remote-c0mpute <remote> <url>`, speak the
/// remote-helper protocol and return the exit code.
pub fn as_remote_helper() -> Option<i32> {
    let mut args = std::env::args_os();
    let argv0 = PathBuf::from(args.next()?);
    let name = argv0.file_name()?.to_string_lossy().to_string();
    if !name.starts_with(HELPER) {
        return None;
    }
    let rest: Vec<String> = args.map(|a| a.to_string_lossy().to_string()).collect();
    let (remote, url) = match rest.as_slice() {
        [r, u, ..] => (r.clone(), u.clone()),
        [u] => (u.clone(), u.clone()),
        _ => {
            eprintln!("usage: {HELPER} <remote> <url>   (git runs this for c0mpute:// URLs)");
            return Some(2);
        }
    };
    Some(match helper::run(&remote, &url) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{HELPER}: {e:#}");
            1
        }
    })
}

/// A directory holding `git-remote-c0mpute` -> this binary, for child gits.
fn helper_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locate this binary")?;
    let dir = c0mpute_core::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("git")
        .join("bin");
    std::fs::create_dir_all(&dir)?;
    link(&exe, &dir.join(HELPER))?;
    Ok(dir)
}

fn link(exe: &std::path::Path, at: &std::path::Path) -> Result<()> {
    if std::fs::read_link(at).ok().as_deref() == Some(exe) {
        return Ok(());
    }
    let _ = std::fs::remove_file(at);
    #[cfg(unix)]
    std::os::unix::fs::symlink(exe, at).with_context(|| format!("link {}", at.display()))?;
    #[cfg(not(unix))]
    std::fs::copy(exe, at).with_context(|| format!("copy to {}", at.display()))?;
    Ok(())
}

/// `git <args>` with the helper on PATH and an optional gateway override.
fn run_git(args: &[String], gateway: Option<&str>) -> Result<()> {
    let dir = helper_dir()?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir];
    paths.extend(std::env::split_paths(&path));
    let mut cmd = Command::new("git");
    cmd.args(args).env("PATH", std::env::join_paths(paths)?);
    if let Some(g) = gateway {
        cmd.env("C0MPUTE_GIT_GATEWAY", g);
    }
    let status = cmd.status().context("run git (is it installed?)")?;
    if !status.success() {
        bail!(
            "git {} failed",
            args.first().map(String::as_str).unwrap_or("")
        );
    }
    Ok(())
}

fn my_key() -> Result<String> {
    let kp = c0mpute_core::hosting::publisher_keypair()?;
    c0mpute_core::hosting::site::owner_hex(&kp).context("this machine's identity is not ed25519")
}

pub async fn run(args: &[String]) -> Result<()> {
    let cli = GitCli::try_parse_from(
        std::iter::once("c0mpute git".to_string()).chain(args.iter().cloned()),
    )
    .unwrap_or_else(|e| e.exit());
    match cli.cmd {
        Sub::Init { repo, remote } => {
            if !git::valid_repo(&repo) {
                bail!(
                    "repo name must be 1-{} of a-z, 0-9 and '-'",
                    git::max_repo_chars()
                );
            }
            let url = git::url_for(&my_key()?, &repo);
            println!("{url}");
            let in_repo = Command::new("git")
                .args(["rev-parse", "--is-inside-work-tree"])
                .output()
                .is_ok_and(|o| o.status.success());
            if in_repo {
                let exists = Command::new("git")
                    .args(["remote", "get-url", &remote])
                    .output()
                    .is_ok_and(|o| o.status.success());
                if exists {
                    eprintln!(
                        "remote {remote:?} already exists; add this one with: git remote add <name> {url}"
                    );
                } else {
                    run_git(
                        &["remote".into(), "add".into(), remote.clone(), url.clone()],
                        None,
                    )?;
                    eprintln!("added remote {remote:?}; push with: c0mpute git push {remote} main");
                }
            }
            eprintln!(
                "run `c0mpute git install-helper` once so plain `git push`/`git clone` work too"
            );
            Ok(())
        }
        Sub::Clone { url, dir, gateway } => {
            let mut a = vec!["clone".to_string(), url];
            a.extend(dir);
            run_git(&a, gateway.as_deref())
        }
        Sub::Push { args, gateway } => {
            let mut a = vec!["push".to_string()];
            a.extend(args);
            run_git(&a, gateway.as_deref())
        }
        Sub::Ls { gateway } => {
            let repos = client::list(&gateway).await?;
            if repos.is_empty() {
                println!("no repos known to {gateway} yet");
            }
            for r in repos {
                println!(
                    "{:<70}  copies {}  {} bytes  {}{}",
                    r["url"].as_str().unwrap_or(""),
                    r["copies"],
                    r["size"],
                    gateway.trim_end_matches('/'),
                    r["clone_path"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
        Sub::Key => {
            println!("{}", my_key()?);
            Ok(())
        }
        Sub::InstallHelper { dir } => {
            let exe = std::env::current_exe()?;
            let dir = match dir {
                Some(d) => d,
                None => exe
                    .parent()
                    .context("binary has no directory")?
                    .to_path_buf(),
            };
            let at = dir.join(HELPER);
            link(&exe, &at)?;
            println!("{}", at.display());
            Ok(())
        }
        // The helper runs its own runtime: off this one's threads.
        Sub::RemoteHelper { remote, url } => std::thread::spawn(move || helper::run(&remote, &url))
            .join()
            .map_err(|_| anyhow::anyhow!("remote helper panicked"))?,
    }
}
