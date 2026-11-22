//! `c0mpute chat plugins ...`: chat plugins (remote MCP servers whose tools
//! the model can use in c0mpute chat) on the signed-in account. The tool loop
//! runs on c0mpute.com, so `c0mpute chat` just uses whatever is on here.
//!
//! ```text
//! c0mpute chat plugins ls                     # yours + the catalog
//! c0mpute chat plugins add threatcrush        # a catalog plugin (opens its sign-in)
//! c0mpute chat plugins add https://x.dev/mcp  # any remote MCP server
//! c0mpute chat plugins on|off <name>          # use it in chat, or not
//! c0mpute chat plugins connect <name>         # sign in again
//! c0mpute chat plugins rm <name>
//! c0mpute chat plugins paper moomoo          # brokerage orders go to the paper account (the default)
//! c0mpute chat plugins live moomoo --confirm # ... or to the live one (real money; each order still asks)
//! ```
//!
//! `c0mpute plugins paper|live <name>` and `/plugin paper|live <name>` in a
//! chat session do the same. A new sign-in resets the plugin to paper, and in
//! paper the server hides the live order tools from the model.

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::ai_chat::{api_error, base_url, client, open_browser, require_key};

const USAGE: &str = "usage: c0mpute chat plugins ls | add <url|name> | rm <name> | on <name> | off <name> | connect <name> | paper <name> | live <name> --confirm";

pub(crate) const LIVE_WARNING: &str = "Live trading places orders with real money. Every order still shows its card and needs your y; the model can be wrong.";

/// `live <slug>` typed without `--confirm`: the slug to ask about.
pub(crate) fn live_needs_confirm(words: &[String]) -> Option<String> {
    match words {
        [verb, slug] if verb == "live" && valid_slug(slug) => Some(slug.clone()),
        _ => None,
    }
}

/// `paper <slug>` / `live <slug> --confirm` as (slug, request body); an error
/// for `live` without `--confirm`; None for any other command.
pub(crate) fn mode_request(words: &[&str]) -> Option<Result<(String, Value)>> {
    let (verb, rest) = words.split_first()?;
    if !matches!(*verb, "paper" | "live") {
        return None;
    }
    let confirm = rest.contains(&"--confirm");
    let slugs: Vec<&&str> = rest.iter().filter(|w| **w != "--confirm").collect();
    let [slug] = slugs.as_slice() else { return Some(Err(anyhow::anyhow!("{USAGE}"))) };
    if !valid_slug(slug) {
        return Some(Err(anyhow::anyhow!("{USAGE}")));
    }
    Some(match *verb {
        "paper" => Ok((slug.to_string(), json!({ "mode": "paper" }))),
        _ if confirm => Ok((slug.to_string(), json!({ "mode": "live", "confirm": true }))),
        _ => Err(anyhow::anyhow!(
            "{LIVE_WARNING}\nto switch {slug} to live trading, run it with --confirm: c0mpute plugins live {slug} --confirm"
        )),
    })
}

/// Switch a brokerage plugin between its paper and live account (same API
/// as the web chat's toggle).
pub async fn set_mode(slug: &str, live: bool, confirm: bool) -> Result<()> {
    let mut words = vec![if live { "live" } else { "paper" }, slug];
    if confirm {
        words.push("--confirm");
    }
    let (slug, body) = mode_request(&words).expect("a mode command")?;
    let h = Http { base: base_url(None), key: require_key()?, http: client()? };
    apply_mode(&h, &slug, body).await
}

async fn apply_mode(h: &Http, slug: &str, body: Value) -> Result<()> {
    let v = h.send(reqwest::Method::POST, &format!("/api/v1/chat/plugins/{slug}/mode"), Some(body)).await?;
    println!("{}", mode_line(slug, v["plugin"]["trading_mode"].as_str().unwrap_or("?")));
    Ok(())
}

pub(crate) fn mode_line(slug: &str, mode: &str) -> String {
    match mode {
        "live" => format!("{slug}: LIVE trading (real money). Every order still shows its card and needs your y."),
        "paper" => format!("{slug}: paper trading (simulated money; live order tools are hidden from the model)"),
        other => format!("{slug}: trading mode {other}"),
    }
}

struct Http {
    base: String,
    key: String,
    http: reqwest::Client,
}

impl Http {
    async fn send(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(&self.key).timeout(Duration::from_secs(60));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error(status, &text));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

/// `{url}` for an https URL, `{slug}` for a catalog name.
pub(crate) fn add_body(target: &str) -> Value {
    if target.starts_with("https://") || target.starts_with("http://") {
        json!({ "url": target })
    } else {
        json!({ "slug": target.to_lowercase() })
    }
}

fn valid_slug(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub async fn run(args: &[String]) -> Result<()> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    if matches!(words.first(), Some(&"-h") | Some(&"--help") | Some(&"help")) {
        println!("{USAGE}");
        return Ok(());
    }
    let h = Http { base: base_url(None), key: require_key()?, http: client()? };
    if let Some(req) = mode_request(&words) {
        let (slug, body) = req?;
        return apply_mode(&h, &slug, body).await;
    }
    match words.as_slice() {
        [] | ["ls"] | ["list"] => ls(&h).await,
        ["add", target] | ["install", target] => add(&h, target).await,
        [verb, slug] if valid_slug(slug) => match *verb {
            "rm" | "remove" | "delete" => {
                h.send(reqwest::Method::DELETE, &format!("/api/v1/chat/plugins/{slug}"), None).await?;
                println!("{slug} removed");
                Ok(())
            }
            "on" | "off" | "enable" | "disable" => {
                let on = matches!(*verb, "on" | "enable");
                let v = h
                    .send(reqwest::Method::POST, &format!("/api/v1/chat/plugins/{slug}/toggle"), Some(json!({ "enabled": on })))
                    .await?;
                println!("{slug} {}", if v["plugin"]["enabled"].as_bool() == Some(true) { "on" } else { "off" });
                Ok(())
            }
            "connect" | "login" | "signin" => {
                let v = h.send(reqwest::Method::GET, &format!("/api/v1/chat/plugins/{slug}/connect"), None).await?;
                sign_in(slug, v["connect_url"].as_str().unwrap_or_default());
                Ok(())
            }
            _ => bail!("{USAGE}"),
        },
        _ => bail!("{USAGE}"),
    }
}

fn sign_in(slug: &str, link: &str) {
    if link.is_empty() {
        println!("{slug}: no sign-in link came back");
        return;
    }
    println!("sign in to {slug} (link valid 10 minutes):\n  {link}");
    open_browser(link);
    println!("its tools turn on in your chats once you finish; check with `c0mpute chat plugins ls`");
}

async fn add(h: &Http, target: &str) -> Result<()> {
    let v = h.send(reqwest::Method::POST, "/api/v1/chat/plugins", Some(add_body(target))).await?;
    let p = &v["plugin"];
    let slug = p["slug"].as_str().unwrap_or("?");
    match v["connect_url"].as_str() {
        Some(link) => {
            println!("{slug} added");
            sign_in(slug, link);
        }
        None => {
            let tools: Vec<&str> = p["tools"].as_array().map(|a| a.iter().filter_map(|t| t["name"].as_str()).collect()).unwrap_or_default();
            println!("{slug} added: {} tools ({})", tools.len(), tools.join(", "));
        }
    }
    Ok(())
}

async fn ls(h: &Http) -> Result<()> {
    let v = h.send(reqwest::Method::GET, "/api/v1/chat/plugins", None).await?;
    let plugins = v["plugins"].as_array().cloned().unwrap_or_default();
    if plugins.is_empty() {
        println!("no chat plugins yet");
    }
    for p in &plugins {
        let mode = match p["trading_mode"].as_str() {
            Some("live") => " [LIVE]",
            Some("paper") => " [paper]",
            _ => "",
        };
        println!(
            "{:<18} {:<14} {:>2} tools  {}{mode}",
            p["slug"].as_str().unwrap_or("?"),
            p["status"].as_str().unwrap_or("?"),
            p["tools"].as_array().map(Vec::len).unwrap_or(0),
            p["url"].as_str().unwrap_or("")
        );
    }
    if plugins.iter().any(|p| p["trading_mode"].is_string()) {
        println!("\nbrokerage: c0mpute plugins paper <name> | live <name> --confirm");
    }
    let catalog: Vec<Value> = v["catalog"].as_array().cloned().unwrap_or_default().into_iter().filter(|c| c["added"].as_bool() != Some(true)).collect();
    if !catalog.is_empty() {
        println!("\ncatalog (c0mpute chat plugins add <name>):");
        for c in catalog {
            println!("  {:<16} {}", c["slug"].as_str().unwrap_or("?"), c["description"].as_str().unwrap_or(""));
        }
    }
    println!("\nany remote MCP server: c0mpute chat plugins add https://…/mcp");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_body_picks_url_or_slug() {
        assert_eq!(add_body("https://x.dev/mcp"), json!({"url": "https://x.dev/mcp"}));
        assert_eq!(add_body("ThreatCrush"), json!({"slug": "threatcrush"}));
    }

    #[test]
    fn paper_and_live_requests() {
        assert_eq!(mode_request(&["paper", "moomoo"]).unwrap().unwrap(), ("moomoo".into(), json!({"mode": "paper"})));
        // paper never sends confirm, even if given
        assert_eq!(mode_request(&["paper", "moomoo", "--confirm"]).unwrap().unwrap().1, json!({"mode": "paper"}));
        assert_eq!(
            mode_request(&["live", "moomoo", "--confirm"]).unwrap().unwrap(),
            ("moomoo".into(), json!({"mode": "live", "confirm": true}))
        );
        assert_eq!(mode_request(&["live", "--confirm", "moomoo"]).unwrap().unwrap().1, json!({"mode": "live", "confirm": true}));
        let e = mode_request(&["live", "moomoo"]).unwrap().unwrap_err().to_string();
        assert!(e.contains("--confirm") && e.contains("real money"), "{e}");
        assert!(mode_request(&["live"]).unwrap().is_err());
        assert!(mode_request(&["paper", "Bad_Slug"]).unwrap().is_err());
        assert!(mode_request(&["ls"]).is_none());
        assert!(mode_request(&["on", "moomoo"]).is_none());
    }

    #[test]
    fn in_chat_live_asks_first() {
        let w = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(live_needs_confirm(&w(&["live", "moomoo"])), Some("moomoo".into()));
        assert_eq!(live_needs_confirm(&w(&["live", "moomoo", "--confirm"])), None);
        assert_eq!(live_needs_confirm(&w(&["paper", "moomoo"])), None);
        assert!(mode_line("moomoo", "live").contains("LIVE"));
        assert!(mode_line("moomoo", "paper").contains("hidden"));
    }

    #[test]
    fn slugs() {
        assert!(valid_slug("threatcrush-2"));
        assert!(!valid_slug("Bad_Slug"));
        assert!(!valid_slug(""));
    }
}
