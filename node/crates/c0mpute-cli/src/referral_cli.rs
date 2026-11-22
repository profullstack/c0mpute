//! Node referrals: a share of the house fee to whoever referred this node.
//!
//! The referral code is the one `install.sh --ref <code>` / `C0MPUTE_REF`
//! already saves in `~/.c0mpute/ref` (shown on c0mpute.com/nodes). When that
//! code is a paid referral code (c0mpute.com/dashboard/refer), the node
//! claims it once, at registration, by signing
//!
//!   c0mpute-referral:v1:<peer_id>:<code>:<unix seconds>
//!
//! with its libp2p identity key and posting it to
//! c0mpute.com/api/v1/referrals/attribute. Infernet, where c0mpute jobs
//! settle, checks the signature (so nobody can attach a code to someone
//! else's node), the self-referral rules and that the node has not settled
//! a paid job yet; the attribution never changes after. The commission comes
//! out of the house fee, never out of this node's earnings.
//!
//! The answer is kept in `~/.c0mpute/ref.claim`, so a code is sent once; a
//! network error leaves nothing there and the next `worker start` retries.
//!
//!   c0mpute worker register --ref <code>
//!   c0mpute referral claim [code]       # claim now (or set + claim a code)
//!   c0mpute referral link | stats       # your own referral link / commissions

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use clap::Subcommand;
use serde_json::{Value, json};

use crate::ai_chat;

#[derive(Subcommand, Debug)]
pub enum ReferralCmd {
    /// Claim this node's referral now (sets the code first when given).
    /// Only before the node's first paid job; never changes after.
    Claim { code: Option<String> },
    /// Your referral link and the install commands that carry your code
    /// (needs `c0mpute chat login`; created once, your commission term is set then).
    Link {
        #[arg(long)]
        json: bool,
    },
    /// Your referred nodes and commissions: pending, confirmed, paid.
    Stats {
        #[arg(long)]
        json: bool,
    },
}

/// A paid referral code is 6-16 lowercase letters/digits; a board handle may
/// also use `-` / `_` (those are never paid codes, so are never sent).
pub fn looks_like_paid_code(code: &str) -> bool {
    (6..=16).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// The exact message the node signs (must match Infernet's c0mputeClaimMessage).
pub fn claim_message(peer_id: &str, code: &str, ts: u64) -> String {
    format!("c0mpute-referral:v1:{peer_id}:{code}:{ts}")
}

/// Save `code` as this node's referral code (`~/.c0mpute/ref`).
pub fn save_code(code: &str) -> Result<String> {
    let code = code.trim().to_ascii_lowercase();
    if code.is_empty() || code.len() > 32 || !code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        bail!("referral code: a-z, 0-9, - or _, up to 32 characters");
    }
    let path = c0mpute_core::capabilities::referral_code_path().ok_or_else(|| anyhow!("HOME not set"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, format!("{code}\n"))?;
    Ok(code)
}

fn answered(code: &str) -> Option<String> {
    let path = c0mpute_core::capabilities::referral_claim_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let mut it = text.split_whitespace();
    (it.next()? == code).then(|| it.collect::<Vec<_>>().join(" "))
}

fn remember(code: &str, outcome: &str) {
    if let Some(path) = c0mpute_core::capabilities::referral_claim_path() {
        let _ = std::fs::write(path, format!("{code} {outcome}\n"));
    }
}

/// Claim the saved referral code if it is a paid code and has no answer yet.
/// Best effort: never fails the caller. `force` re-sends even if answered.
pub async fn claim_if_pending(force: bool, quiet: bool) -> Option<String> {
    let code = c0mpute_core::capabilities::referral_code()?;
    if !looks_like_paid_code(&code) {
        return None;
    }
    if !force {
        if let Some(prev) = answered(&code) {
            return Some(prev);
        }
    }
    match claim(&code).await {
        Ok((outcome, message)) => {
            remember(&code, &outcome);
            if !quiet {
                println!("referral: {message}");
            }
            Some(outcome)
        }
        Err(e) => {
            if !quiet {
                eprintln!("referral: could not reach c0mpute.com ({e}); the claim is retried on the next `c0mpute worker start`");
            }
            None
        }
    }
}

async fn claim(code: &str) -> Result<(String, String)> {
    let dir = c0mpute_core::config::config_dir().ok_or_else(|| anyhow!("no config dir"))?;
    let keypair = c0mpute_net::identity::load_or_create(&dir)?;
    let peer_id = keypair.public().to_peer_id().to_string();
    let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let sig = keypair
        .sign(claim_message(&peer_id, code, ts).as_bytes())
        .map_err(|e| anyhow!("sign claim: {e}"))?;
    let body = json!({
        "peer_id": peer_id,
        "code": code,
        "ts": ts,
        "sig": base64::engine::general_purpose::STANDARD.encode(sig),
    });
    let url = format!("{}/api/v1/referrals/attribute", ai_chat::base_url(None));
    let mut req = ai_chat::client()?.post(url).json(&body).timeout(Duration::from_secs(15));
    // With the operator's chat key, Infernet can refuse a self-referral by account.
    if let Some(key) = ai_chat::api_key() {
        req = req.bearer_auth(key);
    }
    let res = req.send().await?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if status.is_server_error() {
        bail!("HTTP {status}");
    }
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    if data.get("attributed").and_then(Value::as_bool) == Some(true) {
        return Ok(("attributed".into(), format!("this node is credited to referral code {code}")));
    }
    let reason = data
        .get("reason")
        .or_else(|| v.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("refused")
        .to_string();
    let message = data
        .get("message")
        .or_else(|| v.get("error"))
        .and_then(|m| m.as_str().or_else(|| m.get("message").and_then(Value::as_str)))
        .unwrap_or("not applied")
        .to_string();
    Ok((reason, format!("code {code} not applied: {message}")))
}

pub async fn run(cmd: ReferralCmd) -> Result<()> {
    match cmd {
        ReferralCmd::Claim { code } => {
            if let Some(c) = code {
                let saved = save_code(&c)?;
                println!("referral code {saved} saved");
            }
            match claim_if_pending(true, false).await {
                Some(_) => Ok(()),
                None if c0mpute_core::capabilities::referral_code().is_none() => {
                    bail!("no referral code set: c0mpute referral claim <code>")
                }
                None => Ok(()),
            }
        }
        ReferralCmd::Link { json } => {
            let v = api("POST").await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                let s = |p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or("-").to_string();
                println!("referral code: {}", s("/code"));
                println!("your commission term: {}", s("/terms/term/label"));
                println!("  {}", s("/links/c0mpute"));
                println!("  {}", s("/links/infernet"));
                println!("install commands that carry your code:");
                println!("  {}", s("/install/c0mpute"));
                println!("  {}", s("/install/c0mpute_register"));
                println!("  {}", s("/install/infernet"));
            }
            Ok(())
        }
        ReferralCmd::Stats { json } => {
            let v = api("GET").await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else if v.get("has_code").and_then(Value::as_bool) != Some(true) {
                println!("no referral link yet: c0mpute referral link");
            } else {
                let n = |p: &str| v.pointer(p).and_then(Value::as_f64).unwrap_or(0.0);
                println!(
                    "code {} · {} referred node(s), {} active in 7 days",
                    v["code"].as_str().unwrap_or("-"),
                    n("/referred_nodes"),
                    n("/active_nodes_7d")
                );
                println!(
                    "pending ${:.6} · confirmed ${:.6} · available ${:.6} · paid ${:.6}",
                    n("/balance/pending_usd"),
                    n("/balance/confirmed_usd"),
                    n("/balance/available_usd"),
                    n("/balance/paid_usd")
                );
            }
            Ok(())
        }
    }
}

async fn api(method: &str) -> Result<Value> {
    let key = ai_chat::require_key()?;
    let url = format!("{}/api/v1/referrals", ai_chat::base_url(None));
    let client = ai_chat::client()?;
    let req = if method == "POST" {
        client.post(url).header("content-type", "application/json").body("{}")
    } else {
        client.get(url)
    };
    let res = req.bearer_auth(key).timeout(Duration::from_secs(20)).send().await?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(ai_chat::api_error(status, &text));
    }
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok(v.get("data").cloned().unwrap_or(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_paid_codes_are_claimed() {
        assert!(looks_like_paid_code("abcd2345"));
        assert!(!looks_like_paid_code("profullstack-x"));
        assert!(!looks_like_paid_code("chovy"));
        assert!(!looks_like_paid_code("my_handle"));
        assert!(!looks_like_paid_code("ABCD2345"));
    }

    #[test]
    fn claim_message_matches_infernet() {
        assert_eq!(claim_message("12D3KooWabc", "abcd2345", 1_700_000_000), "c0mpute-referral:v1:12D3KooWabc:abcd2345:1700000000");
    }

    #[test]
    fn signature_verifies_against_the_peer_id_key() {
        let kp = libp2p::identity::Keypair::generate_ed25519();
        let msg = claim_message(&kp.public().to_peer_id().to_string(), "abcd2345", 1);
        let sig = kp.sign(msg.as_bytes()).unwrap();
        assert_eq!(sig.len(), 64);
        assert!(kp.public().verify(msg.as_bytes(), &sig));
        // The peer id embeds the ed25519 key (identity multihash), which is what Infernet reads.
        let bytes = kp.public().to_peer_id().to_bytes();
        assert_eq!(&bytes[..6], &[0x00, 36, 0x08, 0x01, 0x12, 0x20]);
    }
}
