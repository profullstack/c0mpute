//! `c0mpute gallery`: get a site listed in the c0mpute gallery
//! (c0mpute.com/gallery, infernetprotocol.com/gallery). A thin client over
//! `/api/v1/gallery/*`; no sign-in needed (a saved key is sent when there is one).
//!
//! ```text
//! c0mpute gallery submit https://example.com --name Example --email you@example.com
//! c0mpute gallery submit example.com --priority --email you@example.com   # $29 via CoinPay
//! c0mpute gallery status https://example.com [--check]
//! c0mpute gallery list [--category apps] [--json]
//! ```

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::ai_chat::{api_error, api_key, base_url, client};

#[derive(Subcommand, Debug, Clone)]
pub enum GalleryCmd {
    /// Submit a site. Free: listed as soon as the c0mpute badge is on it (the
    /// snippet is printed), else after a 30-day queue. --priority: one
    /// CoinPay payment, listed after automated checks.
    Submit(SubmitArgs),
    /// A listing's status by URL or id (gl_...); --check looks for the badge now.
    Status {
        /// The site URL or listing id.
        target: String,
        /// Look for the badge on the site now (once a minute).
        #[arg(long)]
        check: bool,
        #[arg(long)]
        json: bool,
        /// c0mpute site URL (default https://c0mpute.com, or $C0MPUTE_URL).
        #[arg(long)]
        site: Option<String>,
    },
    /// What is listed (the public feed).
    List {
        /// apps, automation, media or models.
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        site: Option<String>,
    },
}

#[derive(Args, Debug, Clone)]
pub struct SubmitArgs {
    /// The site's public URL (the page to list and check for the badge).
    pub url: String,
    #[arg(long)]
    pub name: Option<String>,
    /// One sentence: what it is and what runs on c0mpute.
    #[arg(long)]
    pub tagline: Option<String>,
    /// apps, automation, media or models (default apps).
    #[arg(long)]
    pub category: Option<String>,
    /// c0mpute features used, comma separated: "c0mpute hosting,fine-tune".
    #[arg(long)]
    pub built_with: Option<String>,
    /// Contact email for go-live and badge notices (required with --priority).
    #[arg(long)]
    pub email: Option<String>,
    /// Priority review: one CoinPay payment (prints the checkout link).
    #[arg(long)]
    pub priority: bool,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub site: Option<String>,
}

/// The submit request body (also used by tests).
pub fn submit_body(a: &SubmitArgs) -> Value {
    let mut b = json!({ "url": a.url, "tier": if a.priority { "priority" } else { "free" } });
    if let Some(v) = &a.name {
        b["name"] = json!(v);
    }
    if let Some(v) = &a.tagline {
        b["tagline"] = json!(v);
    }
    if let Some(v) = &a.category {
        b["category"] = json!(v);
    }
    if let Some(v) = &a.built_with {
        let list: Vec<&str> = v.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        b["built_with"] = json!(list);
    }
    if let Some(v) = &a.email {
        b["contact_email"] = json!(v);
    }
    b
}

/// A listing id looks like gl_...; anything else is a URL.
pub fn is_listing_id(s: &str) -> bool {
    s.starts_with("gl_") || s.starts_with("seed-")
}

/// Human lines for a listing.
pub fn describe(l: &Value) -> Vec<String> {
    let s = |k: &str| l.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut out = vec![format!("{}  {}", s("name"), s("url"))];
    let status = s("status");
    let line = match status.as_str() {
        "queued" => format!("queued: goes live {} (or as soon as the badge is found)", s("goes_live_at").get(..10).unwrap_or("")),
        "approved" => format!("listed since {} via {}: {}", s("added_at"), s("listed_via"), s("gallery_url")),
        "awaiting_payment" => "waiting for payment (priority review)".to_string(),
        "priority_review" => "paid, held for a person".to_string(),
        "review" => "waiting for a person".to_string(),
        "rejected" => format!("not listed{}", l.get("note").and_then(Value::as_str).map(|n| format!(": {n}")).unwrap_or_default()),
        other => other.to_string(),
    };
    out.push(format!("status: {line}"));
    if let Some(b) = l.pointer("/badge/state").and_then(Value::as_str) {
        out.push(format!("badge:  {b}"));
    }
    out.push(format!("page:   {}", s("status_url")));
    out
}

async fn send(req: reqwest::RequestBuilder) -> Result<(reqwest::StatusCode, String)> {
    let req = match api_key() {
        Some(k) => req.bearer_auth(k),
        None => req,
    };
    let res = req.send().await.context("could not reach c0mpute.com")?;
    let status = res.status();
    Ok((status, res.text().await.unwrap_or_default()))
}

pub async fn run(cmd: GalleryCmd) -> Result<()> {
    let http = client()?;
    match cmd {
        GalleryCmd::Submit(a) => {
            let site = base_url(a.site.as_deref());
            let (status, text) = send(http.post(format!("{site}/api/v1/gallery/submissions")).json(&submit_body(&a))).await?;
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if status.as_u16() == 409 {
                if a.json {
                    println!("{text}");
                } else if let Some(l) = v.get("listing") {
                    eprintln!("already submitted:");
                    for line in describe(l) {
                        println!("{line}");
                    }
                }
                return Ok(());
            }
            if !status.is_success() {
                return Err(api_error(status, &text));
            }
            if a.json {
                println!("{text}");
                return Ok(());
            }
            println!("{}", v.get("message").and_then(Value::as_str).unwrap_or("submitted"));
            if let Some(l) = v.get("listing") {
                for line in describe(l) {
                    println!("{line}");
                }
                if let Some(url) = v.get("checkout_url").and_then(Value::as_str) {
                    println!("\npay here (priority review): {url}");
                } else if l.get("status").and_then(Value::as_str) != Some("approved") {
                    if let Some(snip) = l.get("snippet").and_then(Value::as_str) {
                        println!("\nput this on your site to go live now, then `c0mpute gallery status {} --check`:\n{snip}", a.url);
                    }
                }
            }
            Ok(())
        }
        GalleryCmd::Status { target, check, json, site } => {
            let site = base_url(site.as_deref());
            let id = if is_listing_id(&target) {
                target.clone()
            } else {
                let (status, text) = send(http.get(format!("{site}/api/v1/gallery/submissions")).query(&[("url", target.as_str())])).await?;
                if !status.is_success() {
                    return Err(api_error(status, &text));
                }
                let v: Value = serde_json::from_str(&text).context("bad response")?;
                if !check {
                    print_listing(&v, &text, json);
                    return Ok(());
                }
                v.pointer("/listing/id").and_then(Value::as_str).context("no listing id in the response")?.to_string()
            };
            let req = if check {
                http.post(format!("{site}/api/v1/gallery/submissions/{id}/check"))
            } else {
                http.get(format!("{site}/api/v1/gallery/submissions/{id}"))
            };
            let (status, text) = send(req).await?;
            if !status.is_success() {
                return Err(api_error(status, &text));
            }
            let v: Value = serde_json::from_str(&text).context("bad response")?;
            if !json {
                if let Some(m) = v.get("message").and_then(Value::as_str) {
                    println!("{m}");
                }
            }
            print_listing(&v, &text, json);
            Ok(())
        }
        GalleryCmd::List { category, json, site } => {
            let site = base_url(site.as_deref());
            let (status, text) = send(http.get(format!("{site}/api/v1/gallery"))).await?;
            if !status.is_success() {
                return Err(api_error(status, &text));
            }
            let v: Value = serde_json::from_str(&text).context("bad response")?;
            let rows: Vec<&Value> = v
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter(|e| category.as_deref().is_none_or(|c| e.get("category").and_then(Value::as_str) == Some(c))).collect())
                .unwrap_or_default();
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            for e in rows {
                let s = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("");
                println!("{:<28} {:<11} {}", s("name"), s("category"), s("url"));
            }
            Ok(())
        }
    }
}

fn print_listing(v: &Value, raw: &str, json: bool) {
    if json {
        println!("{raw}");
    } else if let Some(l) = v.get("listing") {
        for line in describe(l) {
            println!("{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_has_tier_and_split_built_with() {
        let a = SubmitArgs {
            url: "example.com".into(),
            name: Some("Ex".into()),
            tagline: None,
            category: None,
            built_with: Some("c0mpute hosting, fine-tune,".into()),
            email: Some("a@b.co".into()),
            priority: true,
            json: false,
            site: None,
        };
        let b = submit_body(&a);
        assert_eq!(b["tier"], "priority");
        assert_eq!(b["built_with"], json!(["c0mpute hosting", "fine-tune"]));
        assert_eq!(b["contact_email"], "a@b.co");
        assert!(b.get("tagline").is_none());
    }

    #[test]
    fn ids_vs_urls() {
        assert!(is_listing_id("gl_abc"));
        assert!(!is_listing_id("https://example.com"));
    }

    #[test]
    fn describes_a_queued_listing() {
        let l = json!({"name":"Ex","url":"https://ex.com","status":"queued","goes_live_at":"2026-12-22T12:00:00Z","badge":{"state":"missing"},"status_url":"https://c0mpute.com/gallery/status/gl_x"});
        let d = describe(&l);
        assert!(d[1].contains("2026-12-22"));
        assert!(d[2].contains("missing"));
    }
}
