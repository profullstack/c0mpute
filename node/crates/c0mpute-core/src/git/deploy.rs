//! Push to deploy: when a push moves HEAD's branch and that commit has a
//! `c0mpute.toml` with an `[app]` table, the helper deploys it as an app
//! ([`crate::apps::deploy`]). `git config c0mpute.deploy false` turns it off.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::client::Local;

/// `c0mpute.deploy` is not set to false.
pub fn enabled(local: &Local) -> bool {
    local
        .git(&["config", "--bool", "c0mpute.deploy"])
        .map(|v| v != "false")
        .unwrap_or(true)
}

/// Deploy `sha` as an app when its tree has `c0mpute.toml` `[app]`; a commit
/// without one is not an app and is left alone.
pub async fn deploy_commit(local: &Local, sha: &str) -> Result<()> {
    let Ok(toml) = local.git(&["show", &format!("{sha}:c0mpute.toml")]) else {
        return Ok(());
    };
    let m: crate::apps::deploy::Manifest = toml::from_str(&toml).context("c0mpute.toml")?;
    let Some(app) = m.app else { return Ok(()) };
    let dir = local
        .git_dir
        .join(format!("c0mpute-deploy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let res = async {
        let tar = dir.join("commit.tar");
        local.git(&["archive", "--format=tar", "-o", &tar.to_string_lossy(), sha])?;
        let tree = dir.join("tree");
        std::fs::create_dir_all(&tree)?;
        let st = std::process::Command::new("tar")
            .arg("-xf")
            .arg(&tar)
            .arg("-C")
            .arg(&tree)
            .status()?;
        if !st.success() {
            bail!("could not unpack the commit");
        }
        eprintln!(
            "c0mpute: deploying app {} from {}",
            app.name,
            &sha[..12.min(sha.len())]
        );
        crate::apps::deploy::deploy(app.options(&tree, Duration::from_secs(600))).await
    }
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    let rec = res?;
    eprintln!(
        "c0mpute: app {} deployed: /a/{}/ on any gateway",
        rec.name, rec.app_id
    );
    Ok(())
}
