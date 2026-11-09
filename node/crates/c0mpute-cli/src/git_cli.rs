//! `c0mpute git init|clone|push|ls|key|install-helper`, and the
//! `git-remote-c0mpute` entry point (this same binary, run under that name).

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use c0mpute_core::git::{self, client, helper};
use clap::{Parser, Subcommand};

const HELPER: &str = "git-remote-c0mpute";

const LONG_ABOUT: &str = "\
Git repos kept by the c0mpute network. A repo is signed by this machine's key,
the network keeps three self-healing copies, and anyone can clone it from any
gateway, with your machine offline. Plain `git` works once the helper is
installed; the `c0mpute git` commands below are shortcuts.";

const GUIDE: &str = "\
QUICK START
  c0mpute git install                  # once: put git-remote-c0mpute on PATH
  cd my-project
  c0mpute git init my-project          # adds remote origin = c0mpute://<your-key>/my-project
  git push origin main                 # publish (signed by your key)

  # anyone, anywhere, no account:
  git clone c0mpute://<owner-key>/my-project
  git clone https://gateway.c0mpute.com/git/<owner-key>/my-project.git   # no c0mpute needed

URLS
  c0mpute://<owner-key>/<repo>         anyone's repo (owner-key: 64 hex, see `c0mpute git key`)
  c0mpute://<repo>                     a repo of THIS machine's key (short form for your own)
  c0mpute://<20-hex site id>           old URLs from c0mpute < 0.3.11 still work
  Repo names: 1-59 of a-z, 0-9 and '-'.

EVERYDAY GIT
  Everything is normal git: push branches and tags, pull, fetch, clone.
  Only the owner key can push. Pushes are fast-forward only; `git push --force`
  (or +refspec) rewrites history, as on any forge.

ACTIONS: PUSH TO DEPLOY
  When a push moves the default branch (HEAD) and that commit has a
  c0mpute.toml with an [app] table, the push also deploys it as a c0mpute app
  (served at /a/<app_id>/ on every gateway). Turn it off per repo:
    git config c0mpute.deploy false
  There is no CI runner (build/test jobs on push) yet.

GATEWAYS
  Pushes and clones go through a gateway over HTTP. Order tried:
    1. --gateway on clone/push/ls, or C0MPUTE_GIT_GATEWAY (comma-separated)
    2. git config c0mpute.gateway (repeatable)
    3. your own node (http://127.0.0.1:7777), then https://gateway.c0mpute.com
  Gateways are untrusted: the helper checks the owner signature and every file hash.
  A brand-new repo can take a few minutes to show up on OTHER gateways (gossip);
  the gateway you pushed to serves it at once.

DEBUG
  C0MPUTE_GIT_TRACE=1 git push ...     phase timings on stderr
  HTTP 507 on push                     that gateway has no room: push via another
  \"no such repo (yet)\"                 the gateway has not heard of it; wait or pick
                                       the gateway you pushed to

Docs: https://github.com/profullstack/c0mpute/blob/master/docs/git-hosting.md";

#[derive(Parser, Debug)]
#[command(
    name = "c0mpute git",
    about = "Git repos kept by the network: signed by your key, three self-healing copies, clone from any gateway",
    long_about = LONG_ABOUT,
    after_help = "Run `c0mpute git --help` for the full guide (URLs, push to deploy, gateways), or `c0mpute git <command> -h`.",
    after_long_help = GUIDE,
    arg_required_else_help = true
)]
struct GitCli {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(Subcommand, Debug)]
enum Sub {
    /// Print the c0mpute:// URL of a repo under this machine's key and, inside
    /// a git work tree, add it as a remote. Nothing is published until you push.
    #[command(after_help = "Example:\n  cd my-project && c0mpute git init my-project && git push origin main")]
    Init {
        /// Repo name: 1-59 of a-z, 0-9 and '-'.
        repo: String,
        /// Remote name to add (skipped if it already exists).
        #[arg(long, default_value = "origin")]
        remote: String,
        /// End-to-end encrypted: the network keeps and serves the repo but
        /// no gateway or host can read it. Share read access with
        /// `c0mpute git share <repo>`.
        #[arg(long)]
        e2e: bool,
    },
    /// Print the URL that gives read access to one of your encrypted repos:
    /// c0mpute://<you>/<repo>#key=<key>. Anyone holding it can clone and
    /// read the repo, so send it like a password.
    Share { repo: String },
    /// `git clone <url> [dir]` with the remote helper on PATH (no install needed).
    #[command(
        after_help = "Examples:\n  c0mpute git clone c0mpute://<owner-key>/my-project\n  c0mpute git clone c0mpute://<owner-key>/my-project ./src --gateway https://gateway.c0mpute.com"
    )]
    Clone {
        /// c0mpute://<owner-key>/<repo>, c0mpute://<repo> (yours) or c0mpute://<site id>.
        url: String,
        /// Directory to clone into (default: the repo name).
        dir: Option<String>,
        /// Gateway(s) to read from, comma-separated (default: C0MPUTE_GIT_GATEWAY,
        /// git config c0mpute.gateway, else your node then https://gateway.c0mpute.com).
        #[arg(long)]
        gateway: Option<String>,
    },
    /// `git push ...` with the remote helper on PATH; every argument goes to git.
    #[command(
        after_help = "Examples:\n  c0mpute git push origin main\n  c0mpute git push --gateway https://gateway.c0mpute.com origin main   (--gateway before git's arguments)\n  c0mpute git push --tags"
    )]
    Push {
        /// Arguments for `git push` (remote, refspecs, --tags, --force, ...).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Gateway(s) to push to, comma-separated. Must come before the git arguments.
        #[arg(long)]
        gateway: Option<String>,
    },
    /// Repos a gateway knows, with live copy counts.
    Ls {
        /// Gateway to ask (default: your node, else https://gateway.c0mpute.com).
        #[arg(long)]
        gateway: Option<String>,
    },
    /// This machine's owner key (the <owner> in c0mpute://<owner>/<repo>).
    Key,
    /// The clone URL of a repo under this machine's key.
    Url { repo: String },
    /// Put `git-remote-c0mpute` (a link to this binary) in a directory on
    /// PATH, so plain `git clone/push c0mpute://...` works.
    #[command(alias = "install")]
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

/// Where web links for a listing gateway should point: the gateway itself,
/// unless it is this machine's loopback node (then the public gateway).
fn web_base(gateway: &str) -> String {
    let g = gateway.trim_end_matches('/');
    let host = g.split("://").nth(1).unwrap_or(g);
    if host.starts_with("127.") || host.starts_with("localhost") || host.starts_with("[::1]") {
        git::PUBLIC_GATEWAY.to_string()
    } else {
        g.to_string()
    }
}

fn human_bytes(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KiB", n as f64 / 1024.0),
        _ => format!("{:.1} MiB", n as f64 / 1_048_576.0),
    }
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
        Sub::Share { repo } => {
            if !git::valid_repo(&repo) {
                bail!("repo name must be 1-{} of a-z, 0-9 and '-'", git::max_repo_chars());
            }
            let kp = c0mpute_core::hosting::publisher_keypair()?;
            let key = git::e2e::RepoKey::derive(&kp, &repo)?;
            let url = format!("{}#key={}", git::url_for(&my_key()?, &repo), key.to_text());
            println!("{url}");
            eprintln!(
                "anyone with this URL can read {repo}: c0mpute git clone '{url}'\n(the #key part never reaches a gateway; this only applies to repos pushed encrypted)"
            );
            Ok(())
        }
        Sub::Init { repo, remote, e2e } => {
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
                if e2e {
                    run_git(
                        &["config".into(), "c0mpute.e2e".into(), "true".into()],
                        None,
                    )?;
                    eprintln!(
                        "end-to-end encrypted: pushes are sealed with this repo's key; give others read access with `c0mpute git share {repo}`"
                    );
                }
            } else if e2e {
                eprintln!(
                    "not inside a git work tree: run this again in the repo, or push to {url}#e2e"
                );
            }
            eprintln!(
                "run `c0mpute git install-helper` once so plain `git push`/`git clone` work too"
            );
            Ok(())
        }
        Sub::Clone { url, dir, gateway } => {
            // git would name the directory after the last URL segment,
            // `#key=...` included: default to the repo name instead.
            let dir = dir.or_else(|| {
                let (base, _, _) = git::e2e::split_fragment(&url).ok()?;
                let u = git::RepoUrl::parse(base).ok()?;
                (!u.repo.is_empty()).then_some(u.repo)
            });
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
            let candidates = match gateway {
                Some(g) => vec![g],
                None => client::Gateways::discover(None).0,
            };
            let mut last = None;
            let mut found = None;
            for g in candidates {
                match client::list(&g).await {
                    Ok(r) => {
                        found = Some((g, r));
                        break;
                    }
                    Err(e) => last = Some(e),
                }
            }
            let Some((gateway, repos)) = found else {
                return Err(last.unwrap_or_else(|| anyhow::anyhow!("no gateway")));
            };
            if repos.is_empty() {
                println!("no repos known to {gateway} yet");
            }
            // Links on a loopback gateway only open on this machine: point
            // them at the public gateway, which knows every repo by gossip.
            let web = web_base(&gateway);
            for r in repos {
                let path = r["clone_path"].as_str().unwrap_or("");
                println!(
                    "{}\n  copies {}/3  {}{}\n  browse  {web}{}\n  clone   git clone {web}{path}",
                    r["url"].as_str().unwrap_or(""),
                    r["copies"],
                    human_bytes(r["size"].as_u64().unwrap_or(0)),
                    if r["copies"].as_u64() == Some(0) {
                        "  (no live copy: unreadable until a holder comes back)"
                    } else {
                        ""
                    },
                    path.strip_suffix(".git").unwrap_or(path),
                );
            }
            Ok(())
        }
        Sub::Key => {
            println!("{}", my_key()?);
            Ok(())
        }
        Sub::Url { repo } => {
            let u = git::RepoUrl::parse(&repo)?;
            if u.site.is_some() {
                bail!("give a repo name, not a site id");
            }
            println!("{}", git::url_for(u.owner_or(&my_key()?), &u.repo));
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
