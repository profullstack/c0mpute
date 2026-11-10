//! `git-remote-c0mpute`: git's remote-helper protocol (gitremote-helpers(7))
//! over [`super::client`]. Git runs it as `git-remote-c0mpute <remote> <url>`
//! for any `c0mpute://` URL, with `GIT_DIR` set, and talks on stdin/stdout.
//!
//! Capabilities: `fetch` (download the signed packs it needs), `push`
//! (sign and send a new repo version to a gateway) and `option` (all
//! declined, which git accepts).
//!
//! Private repos (the default for new ones) are opened with this machine's
//! storage identity, loaded through the [`client::SecretLoader`] the CLI
//! passes in, only when a repo turns out to be private.

use std::io::{BufRead, Write};

use anyhow::{Context, Result};

use super::client::{self, Gateways, Local, PushOptions, PushSpec, SecretLoader};
use super::{RepoUrl, Verified};

/// Run the helper protocol until git closes stdin or sends a blank line.
pub fn run(remote: &str, url: &str, secret: SecretLoader<'_>) -> Result<()> {
    // Multi-threaded: a push-to-deploy boots a libp2p node.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?;
    let res = serve(&rt, remote, url, secret);
    // A deploy's swarm keeps tasks alive; do not wait for them.
    rt.shutdown_timeout(std::time::Duration::from_millis(200));
    res
}

/// The repo's state with a private repo's refs opened.
fn load(
    rt: &tokio::runtime::Runtime,
    gws: &Gateways,
    owner: &str,
    repo: &str,
    secret: SecretLoader<'_>,
) -> Result<Option<(Verified, String)>> {
    let mut s = rt.block_on(client::fetch_state(gws, owner, repo))?;
    if let Some(st) = s.as_mut()
        && st.0.doc.sealed
    {
        let sec = secret()?;
        rt.block_on(client::unseal(gws, st, owner, repo, &sec))?;
    }
    Ok(s)
}

/// `git config c0mpute.public`: `Some(true)` makes a new repo public.
fn public_setting(local: &Local) -> Option<bool> {
    match local.git(&["config", "--bool", "c0mpute.public"]).ok()?.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn serve(
    rt: &tokio::runtime::Runtime,
    _remote: &str,
    url: &str,
    secret: SecretLoader<'_>,
) -> Result<()> {
    let target = RepoUrl::parse(url)?;
    let keypair = crate::hosting::publisher_keypair()?;
    let me = crate::hosting::site::owner_hex(&keypair).context("ed25519 identity")?;
    let local = Local::discover()?;
    let gws = Gateways::discover(Some(&local.git_dir));
    let (owner, repo) = match &target.site {
        Some(id) => rt.block_on(client::resolve_site(&gws, id))?,
        None => (target.owner_or(&me).to_string(), target.repo.clone()),
    };

    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    let mut lines = stdin.lock().lines();
    let mut state: Option<Option<(Verified, String)>> = None;

    while let Some(line) = lines.next() {
        let line = line?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if line == "capabilities" {
            out.write_all(b"fetch\npush\noption\n\n")?;
        } else if line.starts_with("option ") {
            out.write_all(b"unsupported\n")?;
        } else if line == "list" || line == "list for-push" {
            let s = load(rt, &gws, &owner, &repo, secret)?;
            if let Some((v, _)) = &s {
                for (name, oid) in &v.doc.refs {
                    writeln!(out, "{oid} {name}")?;
                }
                if let Some(h) = &v.doc.head
                    && v.doc.refs.contains_key(h)
                {
                    writeln!(out, "@{h} HEAD")?;
                }
            }
            out.write_all(b"\n")?;
            state = Some(s);
        } else if let Some(first) = line.strip_prefix("fetch ") {
            let mut wants = vec![first.split(' ').next().unwrap_or("").to_string()];
            for l in lines.by_ref() {
                let l = l?;
                let l = l.trim_end();
                if l.is_empty() {
                    break;
                }
                if let Some(rest) = l.strip_prefix("fetch ") {
                    wants.push(rest.split(' ').next().unwrap_or("").to_string());
                }
            }
            let s = match state.take() {
                Some(s) => s,
                None => load(rt, &gws, &owner, &repo, secret)?,
            };
            let Some(s) = s else {
                anyhow::bail!("{} does not exist", super::url_for(&owner, &repo));
            };
            let n = rt.block_on(client::fetch_objects(
                &local, &gws, &s, &owner, &repo, &wants,
            ))?;
            if n > 0 {
                eprintln!(
                    "c0mpute: fetched {n} pack(s) of {} from {}",
                    super::url_for(&owner, &repo),
                    s.1
                );
            }
            state = Some(Some(s));
            out.write_all(b"\n")?;
        } else if let Some(first) = line.strip_prefix("push ") {
            let mut specs = vec![PushSpec::parse(first)?];
            for l in lines.by_ref() {
                let l = l?;
                let l = l.trim_end();
                if l.is_empty() {
                    break;
                }
                if let Some(rest) = l.strip_prefix("push ") {
                    specs.push(PushSpec::parse(rest)?);
                }
            }
            let opts = PushOptions {
                secret: Some(secret),
                public: public_setting(&local),
                ..Default::default()
            };
            // Sign with the key that owns the repo: this machine's own, or
            // one added with `c0mpute git key import`.
            let pushed = match super::keys::keypair_for(&owner) {
                Ok(kp) => rt.block_on(client::push(
                    &local, &gws, &kp, &owner, &repo, &specs, &opts,
                )),
                Err(e) => Err(e),
            };
            match pushed {
                Ok(report) => {
                    if let Some(gw) = &report.gateway {
                        eprintln!(
                            "c0mpute: {} is now version {} (site {}), accepted by {gw}; the network keeps 3 copies",
                            super::url_for(&owner, &repo),
                            report.seq,
                            report.site_id
                        );
                    }
                    for (dst, r) in &report.results {
                        match r {
                            Ok(()) => writeln!(out, "ok {dst}")?,
                            Err(e) => writeln!(out, "error {dst} {}", e.replace('\n', " "))?,
                        }
                    }
                    if report.gateway.is_some()
                        && let Some(sha) = &report.head_moved
                        && super::deploy::enabled(&local)
                        && let Err(e) = rt.block_on(super::deploy::deploy_commit(&local, sha))
                    {
                        eprintln!("c0mpute: deploy failed: {e:#}");
                    }
                }
                Err(e) => {
                    let msg = format!("{e:#}").replace('\n', " ");
                    for s in &specs {
                        writeln!(out, "error {} {msg}", s.dst)?;
                    }
                }
            }
            out.write_all(b"\n")?;
            state = None;
        } else {
            anyhow::bail!("git-remote-c0mpute: unknown command {line:?}");
        }
        out.flush()?;
    }
    out.flush()?;
    Ok(())
}
