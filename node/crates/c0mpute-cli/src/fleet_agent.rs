//! The worker side of fleet management: a linked node polls c0mpute.com for
//! commands its operator queued (pause, resume, update, restart, and the node
//! settings: models, house-models, uncensored, hosting, storage), runs the
//! ones that check out, and reports back. Each poll also carries the node's
//! state (settings, models served, storage/hosting, disk, a failed update) so
//! the fleet view and the account's alerts see it; the state's SHA-256 is
//! inside the signed poll message.
//!
//! A command runs only when all of these hold:
//!   - it is signed with the fleet key this node pinned when it linked
//!     (`<config dir>/account.json`), over the exact text in
//!     `command_message` (same as the site's `commandMessage`);
//!   - it names this node's peer id and the account it is linked to;
//!   - it has not expired and has not run here before.
//! Fetching and reporting are signed with the node's identity key, so only
//! this node can see or answer its commands.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::account_cli::{LinkRecord, load_link};
use crate::ai_chat::client;

const POLL: Duration = Duration::from_secs(60);
const DONE_FILE: &str = "fleet-done";
const DONE_KEEP: usize = 500;

/// The exact text c0mpute.com signs for one command (apps/web/src/lib/accounts/fleet.ts `commandMessage`).
pub fn command_message(id: &str, account_id: &str, peer_id: &str, action: &str, args: &str, expires_at: &str) -> String {
    format!("c0mpute fleet command v1\nid: {id}\naccount: {account_id}\nnode: {peer_id}\naction: {action}\nargs: {args}\nexpires: {expires_at}")
}

/// What the node signs to fetch or report (fleet.ts `pollMessage`).
pub fn poll_message(site: &str, peer_id: &str, ts: u64, result: Option<(&str, bool)>) -> String {
    match result {
        None => format!("c0mpute node poll v1\nsite: {site}\nnode: {peer_id}\nts: {ts}"),
        Some((id, ok)) => format!("c0mpute node result v1\nsite: {site}\nnode: {peer_id}\nts: {ts}\nid: {id}\nok: {ok}"),
    }
}

/// A poll that carries the node's state: the poll message plus the state's
/// SHA-256 (fleet.ts `pollMessage` with `stateSha256`).
pub fn poll_message_with_state(site: &str, peer_id: &str, ts: u64, state: &str) -> String {
    use sha2::Digest;
    format!("{}\nstate: {}", poll_message(site, peer_id, ts, None), hex::encode(sha2::Sha256::digest(state.as_bytes())))
}

/// Most served models listed in the reported state.
const STATE_MODELS: usize = 64;

/// What this node reports about itself on each poll (JSON).
pub fn node_state() -> Value {
    use c0mpute_core::fleet_settings as fs;
    let settings = fs::current();
    let eff = fs::effective();
    let mut served = c0mpute_core::inference::ready_models();
    served.truncate(STATE_MODELS);
    let disk = fs::disk(eff.as_ref().and_then(|e| e.storage_root.as_deref())).map(|(free, total)| json!({"free_bytes": free, "total_bytes": total}));
    let update_error = c0mpute_update::last_install_error().map(|(version, error, at)| json!({"version": version, "error": error, "at": at}));
    json!({
        "v": 1,
        "version": env!("CARGO_PKG_VERSION"),
        "paused": c0mpute_core::fleet_pause::paused(),
        "settings": settings,
        "effective": {
            "served_models": served,
            "house_models": settings.house_models != Some(false),
            "uncensored": settings.uncensored != Some(false),
            "storage": eff.as_ref().map(|e| e.storage),
            "hosting": eff.as_ref().map(|e| e.hosting),
        },
        "disk": disk,
        "update_error": update_error,
    })
}

/// Apply one settings command (models, house-models, uncensored, hosting,
/// storage). Returns (report text, whether the worker must restart for it).
pub fn apply_setting(action: &str, args: &str) -> std::result::Result<(String, bool), String> {
    use c0mpute_core::fleet_settings as fs;
    let parsed: Value = serde_json::from_str(if args.trim().is_empty() { "{}" } else { args }).map_err(|_| "args are not JSON".to_string())?;
    let cur = fs::current();
    let next = fs::applied(&cur, action, &parsed)?;
    let changed = next != cur;
    if changed {
        fs::save(&next).map_err(|e| format!("could not save fleet-settings.json: {e}"))?;
    }
    let restart = changed && fs::needs_restart(action);
    let mut served = c0mpute_core::inference::ready_models();
    served.truncate(STATE_MODELS);
    let note = if restart {
        "restarting after running jobs finish so the worker picks it up"
    } else if changed {
        "in effect now"
    } else {
        "already set"
    };
    let report = json!({
        "action": action,
        "changed": changed,
        "settings": next,
        "served_models": served,
        "restart": restart,
        "note": note,
    });
    Ok((report.to_string(), restart))
}

/// Unix seconds of an ISO-8601 UTC time like `2026-11-23T10:00:00.000Z`.
pub fn iso_to_unix(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || !s.ends_with('Z') {
        return None;
    }
    let n = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0, 4)?, n(5, 7)?, n(8, 10)?, n(11, 13)?, n(14, 16)?, n(17, 19)?);
    // days from civil (Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let t = days * 86_400 + hh * 3600 + mm * 60 + ss;
    u64::try_from(t).ok()
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Why a command must not run, or None when it checks out.
pub fn refuse(c: &Value, rec: &LinkRecord, now: u64) -> Option<String> {
    let s = |k: &str| c[k].as_str().unwrap_or("");
    let (id, account, peer, action, args, expires) = (s("id"), s("account_id"), s("peer_id"), s("action"), s("args"), s("expires_at"));
    if id.is_empty() || action.is_empty() {
        return Some("malformed command".into());
    }
    if account != rec.account_id {
        return Some(format!("for account {account}, but this node is linked to {}", rec.account_id));
    }
    if peer != rec.peer_id {
        return Some("for another node".into());
    }
    match iso_to_unix(expires) {
        Some(t) if t > now => {}
        _ => return Some("expired".into()),
    }
    let key = match base64::engine::general_purpose::STANDARD.decode(&rec.fleet_key) {
        Ok(k) => k,
        Err(_) => return Some("pinned fleet key is unreadable; run `c0mpute node link` again".into()),
    };
    let sig = match base64::engine::general_purpose::STANDARD.decode(s("sig")) {
        Ok(k) => k,
        Err(_) => return Some("bad signature encoding".into()),
    };
    let pk = match c0mpute_net::libp2p::identity::ed25519::PublicKey::try_from_bytes(&key) {
        Ok(pk) => pk,
        Err(_) => return Some("pinned fleet key is not an ed25519 key".into()),
    };
    if !pk.verify(command_message(id, account, peer, action, args, expires).as_bytes(), &sig) {
        return Some("signature does not match the pinned c0mpute.com fleet key".into());
    }
    None
}

fn done_path() -> Option<std::path::PathBuf> {
    c0mpute_core::config::config_dir().map(|d| d.join(DONE_FILE))
}

fn already_done(id: &str) -> bool {
    done_path().and_then(|p| std::fs::read_to_string(p).ok()).is_some_and(|s| s.lines().any(|l| l == id))
}

fn mark_done(id: &str) {
    let Some(p) = done_path() else { return };
    let mut ids: Vec<String> = std::fs::read_to_string(&p).unwrap_or_default().lines().map(str::to_string).collect();
    ids.push(id.to_string());
    let skip = ids.len().saturating_sub(DONE_KEEP);
    let _ = std::fs::write(&p, ids[skip..].join("\n") + "\n");
}

/// Start polling in the background (worker start). Does nothing until the node is linked.
pub fn spawn() {
    if std::env::var("C0MPUTE_FLEET").as_deref() == Ok("0") {
        info!("fleet commands off (C0MPUTE_FLEET=0)");
        return;
    }
    if c0mpute_core::fleet_pause::paused() {
        warn!("this node is paused from its fleet: not bidding on new jobs (`c0mpute fleet resume` from the account)");
    }
    tokio::spawn(async {
        // Spread the first poll so a fleet restarting together does not arrive at once.
        let jitter = (std::process::id() as u64 * 7919) % 30;
        tokio::time::sleep(Duration::from_secs(5 + jitter)).await;
        loop {
            if let Some(rec) = load_link() {
                if let Err(e) = poll_once(&rec).await {
                    debug!(err = %format!("{e:#}"), "fleet poll failed");
                }
            }
            tokio::time::sleep(POLL).await;
        }
    });
}

async fn signed_post(site: &str, path: &str, mut body: Value, msg: &str) -> Result<Value> {
    let dir = c0mpute_core::config::config_dir().ok_or_else(|| anyhow!("no config dir"))?;
    let kp = c0mpute_net::identity::load_or_create(&dir)?;
    let sig = kp.sign(msg.as_bytes()).map_err(|e| anyhow!("sign: {e}"))?;
    body["signature"] = json!(base64::engine::general_purpose::STANDARD.encode(sig));
    let res = client()?.post(format!("{site}{path}")).json(&body).timeout(Duration::from_secs(20)).send().await?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        anyhow::bail!("HTTP {status}: {}", v["error"]["message"].as_str().unwrap_or(""));
    }
    Ok(v)
}

async fn report(rec: &LinkRecord, id: &str, ok: bool, output: &str) {
    let ts = now_s();
    let msg = poll_message(&rec.site, &rec.peer_id, ts, Some((id, ok)));
    let body = json!({"peer_id": rec.peer_id, "ts": ts, "id": id, "ok": ok, "output": output});
    if let Err(e) = signed_post(&rec.site, "/api/v1/nodes/commands/result", body, &msg).await {
        warn!(err = %format!("{e:#}"), id, "fleet: could not report a command result");
    }
}

async fn poll_once(rec: &LinkRecord) -> Result<()> {
    let dir = c0mpute_core::config::config_dir().context("no config dir")?;
    let kp = c0mpute_net::identity::load_or_create(&dir)?;
    if kp.public().to_peer_id().to_string() != rec.peer_id {
        warn!("fleet: this node's identity changed since it was linked; run `c0mpute node link` again");
        return Ok(());
    }
    let ts = now_s();
    let state = node_state().to_string();
    let msg = poll_message_with_state(&rec.site, &rec.peer_id, ts, &state);
    let v = signed_post(&rec.site, "/api/v1/nodes/commands", json!({"peer_id": rec.peer_id, "ts": ts, "state": state}), &msg).await?;
    let commands = v["commands"].as_array().cloned().unwrap_or_default();
    for c in commands {
        let id = c["id"].as_str().unwrap_or("").to_string();
        if id.is_empty() || already_done(&id) {
            continue;
        }
        if let Some(why) = refuse(&c, rec, now_s()) {
            warn!(id, why, "fleet: refusing a command");
            mark_done(&id);
            report(rec, &id, false, &format!("refused by the node: {why}")).await;
            continue;
        }
        let action = c["action"].as_str().unwrap_or("");
        info!(id, action, "fleet: running a command from the operator");
        mark_done(&id);
        match action {
            "pause" | "resume" => {
                let on = action == "pause";
                match c0mpute_core::fleet_pause::set(on) {
                    Ok(()) => report(rec, &id, true, if on { "paused: not bidding on new jobs" } else { "resumed: bidding again" }).await,
                    Err(e) => report(rec, &id, false, &format!("could not save the pause state: {e}")).await,
                }
            }
            "restart" => {
                report(rec, &id, true, "restarting after running jobs finish").await;
                #[cfg(unix)]
                {
                    let e = c0mpute_update::drain_and_reexec().await;
                    warn!(err = %e, "fleet: restart failed");
                }
            }
            "update" => {
                let feed = std::env::var("C0MPUTE_UPDATE_FEED").unwrap_or_else(|_| c0mpute_update::DEFAULT_RELEASE_FEED.to_string());
                match c0mpute_update::upgrade_with(env!("CARGO_PKG_VERSION"), &feed, c0mpute_update::PostBench::Detached).await {
                    Ok(c0mpute_update::UpgradeOutcome::Upgraded { from, to }) => {
                        report(rec, &id, true, &format!("updated {from} -> {to}; restarting after running jobs finish")).await;
                        #[cfg(unix)]
                        {
                            let e = c0mpute_update::drain_and_reexec().await;
                            warn!(err = %e, "fleet: restart after update failed; the new version runs on the next restart");
                        }
                    }
                    Ok(_) => report(rec, &id, true, &format!("already on the latest release ({})", env!("CARGO_PKG_VERSION"))).await,
                    Err(e) => {
                        c0mpute_update::record_install_error("latest", &format!("{e:#}"));
                        report(rec, &id, false, &format!("update failed: {e:#}")).await
                    }
                }
            }
            a if c0mpute_core::fleet_settings::handles(a) => match apply_setting(a, c["args"].as_str().unwrap_or("{}")) {
                Ok((out, restart)) => {
                    report(rec, &id, true, &out).await;
                    if restart {
                        #[cfg(unix)]
                        {
                            let e = c0mpute_update::drain_and_reexec().await;
                            warn!(err = %e, "fleet: restart for a new setting failed; it applies on the next restart");
                        }
                    }
                }
                Err(why) => report(rec, &id, false, &format!("refused by the node: {why}")).await,
            },
            other => report(rec, &id, false, &format!("this node (v{}) does not support '{other}' yet", env!("CARGO_PKG_VERSION"))).await,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(key: &c0mpute_net::libp2p::identity::ed25519::Keypair) -> LinkRecord {
        LinkRecord {
            site: "https://c0mpute.com".into(),
            account_id: "u_1".into(),
            peer_id: "12D3KooWnode".into(),
            fleet_key: base64::engine::general_purpose::STANDARD.encode(key.public().to_bytes()),
            linked_at: 0,
        }
    }

    fn cmd(key: &c0mpute_net::libp2p::identity::ed25519::Keypair, account: &str, peer: &str, action: &str, expires: &str) -> Value {
        let sig = key.sign(command_message("fc_1", account, peer, action, "{}", expires).as_bytes());
        json!({"id": "fc_1", "account_id": account, "peer_id": peer, "action": action, "args": "{}", "expires_at": expires, "sig": base64::engine::general_purpose::STANDARD.encode(sig)})
    }

    #[test]
    fn iso_times() {
        assert_eq!(iso_to_unix("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(iso_to_unix("2026-11-23T10:00:00.000Z"), Some(1_795_428_000));
        assert_eq!(iso_to_unix("2000-02-29T12:34:56Z"), Some(951_827_696));
        assert_eq!(iso_to_unix("garbage"), None);
    }

    #[test]
    fn only_signed_current_commands_for_this_node_and_account_run() {
        let site = c0mpute_net::libp2p::identity::ed25519::Keypair::generate();
        let r = rec(&site);
        let future = "2099-01-01T00:00:00.000Z";
        let now = 1_800_000_000;
        assert_eq!(refuse(&cmd(&site, "u_1", "12D3KooWnode", "pause", future), &r, now), None);
        // another account, another node, expired
        assert!(refuse(&cmd(&site, "u_2", "12D3KooWnode", "pause", future), &r, now).unwrap().contains("account"));
        assert!(refuse(&cmd(&site, "u_1", "12D3KooWother", "pause", future), &r, now).unwrap().contains("another node"));
        assert_eq!(refuse(&cmd(&site, "u_1", "12D3KooWnode", "pause", "2020-01-01T00:00:00.000Z"), &r, now).as_deref(), Some("expired"));
        // signed by a key that is not the pinned one
        let evil = c0mpute_net::libp2p::identity::ed25519::Keypair::generate();
        assert!(refuse(&cmd(&evil, "u_1", "12D3KooWnode", "pause", future), &r, now).unwrap().contains("signature"));
        // tampered after signing
        let mut c = cmd(&site, "u_1", "12D3KooWnode", "pause", future);
        c["action"] = json!("restart");
        assert!(refuse(&c, &r, now).unwrap().contains("signature"));
    }

    #[test]
    fn settings_commands_are_signed_like_any_other() {
        // The args are inside the signed text: changing them breaks the signature.
        let site = c0mpute_net::libp2p::identity::ed25519::Keypair::generate();
        let r = rec(&site);
        let future = "2099-01-01T00:00:00.000Z";
        let signed = |id: &str, action: &str, args: &str| {
            let sig = site.sign(command_message(id, "u_1", "12D3KooWnode", action, args, future).as_bytes());
            json!({"id": id, "account_id": "u_1", "peer_id": "12D3KooWnode", "action": action, "args": args, "expires_at": future, "sig": base64::engine::general_purpose::STANDARD.encode(sig)})
        };
        let mut c = signed("fc_2", "hosting", r#"{"on":false}"#);
        assert_eq!(refuse(&c, &r, 1_800_000_000), None);
        c["args"] = json!(r#"{"on":true}"#);
        assert!(refuse(&c, &r, 1_800_000_000).unwrap().contains("signature"));
        let mut m = signed("fc_3", "models", r#"{"models":["qwen3:4b"]}"#);
        assert_eq!(refuse(&m, &r, 1_800_000_000), None);
        m["args"] = json!(r#"{"models":["evil:1b"]}"#);
        assert!(refuse(&m, &r, 1_800_000_000).unwrap().contains("signature"));
    }

    #[test]
    fn bad_setting_args_are_refused_before_anything_is_saved() {
        assert!(apply_setting("hosting", "not json").is_err());
        assert!(apply_setting("hosting", r#"{"on":"yes"}"#).is_err());
        assert!(apply_setting("models", r#"{"models":["a b"]}"#).is_err());
    }

    #[test]
    fn state_hash_is_in_the_signed_poll() {
        let m = poll_message_with_state("https://c0mpute.com", "P", 5, "{}");
        // sha256("{}")
        assert_eq!(m, "c0mpute node poll v1\nsite: https://c0mpute.com\nnode: P\nts: 5\nstate: 44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a");
        let s = node_state();
        assert_eq!(s["v"], 1);
        assert!(s["settings"].is_object() && s["effective"]["served_models"].is_array());
    }

    #[test]
    fn messages_match_the_site() {
        assert_eq!(poll_message("https://c0mpute.com", "P", 5, None), "c0mpute node poll v1\nsite: https://c0mpute.com\nnode: P\nts: 5");
        assert_eq!(poll_message("https://c0mpute.com", "P", 5, Some(("fc_1", true))), "c0mpute node result v1\nsite: https://c0mpute.com\nnode: P\nts: 5\nid: fc_1\nok: true");
    }
}
