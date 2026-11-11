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
//! ```

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::ai_chat::{api_error, base_url, client, open_browser, require_key};

const USAGE: &str = "usage: c0mpute chat plugins ls | add <url|name> | rm <name> | on <name> | off <name> | connect <name>";

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
        println!(
            "{:<18} {:<14} {:>2} tools  {}",
            p["slug"].as_str().unwrap_or("?"),
            p["status"].as_str().unwrap_or("?"),
            p["tools"].as_array().map(Vec::len).unwrap_or(0),
            p["url"].as_str().unwrap_or("")
        );
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
    fn slugs() {
        assert!(valid_slug("threatcrush-2"));
        assert!(!valid_slug("Bad_Slug"));
        assert!(!valid_slug(""));
    }
}
