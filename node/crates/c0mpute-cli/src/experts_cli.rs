//! `c0mpute experts` and `c0mpute ask`: the c0mpute Experts store
//! (c0mpute.com/experts). A thin client over `/api/v1/experts*`.
//!
//! ```text
//! c0mpute experts                         # list experts and prices (no sign-in needed)
//! c0mpute experts status                  # your trial, plans, per-expert access
//! c0mpute experts trial                   # start the free trial now (else the first question does)
//! c0mpute experts subscribe all --email you@example.com   # prints a CoinPay checkout link
//! c0mpute ask security-analyst "Is CVE-2024-45337 urgent for a TLS-only Go service?"
//! echo "review this" | c0mpute ask automation-engineer -   # question from stdin
//! ```

use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::io::Read;

use crate::ai_chat::{api_error, api_key, base_url, client, require_key};

#[derive(Subcommand, Debug, Clone)]
pub enum ExpertsCmd {
    /// The experts and their prices (the default).
    List {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        site: Option<String>,
    },
    /// Your trial (days left), paid plans and which experts you can ask.
    Status {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        site: Option<String>,
    },
    /// Start your free trial now (one per CoinPay account, covering every expert).
    Trial {
        #[arg(long)]
        site: Option<String>,
    },
    /// Open a CoinPay checkout for one period: `all` or an expert slug.
    Subscribe {
        /// "all" or an expert slug (security-analyst, sales-coach, ...).
        plan: String,
        /// Where CoinPay sends the receipt.
        #[arg(long)]
        email: String,
        #[arg(long)]
        site: Option<String>,
    },
}

#[derive(Args, Debug, Clone)]
pub struct AskArgs {
    /// Expert slug (see `c0mpute experts`), e.g. security-analyst.
    pub expert: String,
    /// The question; `-` (or nothing) reads it from stdin.
    #[arg(trailing_var_arg = true)]
    pub question: Vec<String>,
    /// Print the raw JSON response.
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub site: Option<String>,
}

/// "expert:tax-info" or "tax-info" → "tax-info".
pub fn slug_of(s: &str) -> String {
    s.trim().trim_start_matches("expert:").to_ascii_lowercase()
}

/// The ask request body.
pub fn ask_body(question: &str) -> Value {
    json!({ "messages": [{ "role": "user", "content": question }] })
}

/// One line per expert for `c0mpute experts`.
pub fn list_lines(v: &Value) -> Vec<String> {
    let mut out = vec![];
    for e in v.get("data").and_then(Value::as_array).into_iter().flatten() {
        let s = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("");
        let price = e.pointer("/store/price_usd").and_then(Value::as_f64).unwrap_or(0.0);
        out.push(format!("{:<22} {:<11} ${:<5} {}", s("slug"), s("category"), price, s("summary")));
    }
    if let Some(p) = v.get("pricing") {
        let n = |k: &str| p.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        out.push(String::new());
        out.push(format!(
            "{} days free (one trial per CoinPay account, every expert), then the price above or ${} for all, per {} days.",
            n("trial_days"),
            n("all_experts_usd"),
            n("period_days")
        ));
        out.push("ask: c0mpute ask <slug> \"your question\"".into());
    }
    out
}

/// Human lines for `c0mpute experts status`.
pub fn status_lines(v: &Value) -> Vec<String> {
    let mut out = vec![];
    if v.get("coinpay").and_then(Value::as_bool) != Some(true) {
        out.push("CoinPay is not connected: connect it at https://c0mpute.com/account to start the free trial.".into());
        return out;
    }
    match v.get("trial") {
        Some(t) if !t.is_null() => {
            let s = |k: &str| t.get(k).and_then(Value::as_str).unwrap_or("").get(..10).unwrap_or("").to_string();
            let left = t.get("days_left").and_then(Value::as_i64).unwrap_or(0);
            if left > 0 {
                out.push(format!("trial: {left} days left (started {}, ends {})", s("started_at"), s("ends_at")));
            } else {
                out.push(format!("trial: ended {}", s("ends_at")));
            }
        }
        _ => out.push("trial: not started (your first question starts it)".into()),
    }
    for p in v.get("subscriptions").and_then(Value::as_array).into_iter().flatten() {
        let plan = p.get("plan").and_then(Value::as_str).unwrap_or("");
        let until = p.get("paid_until").and_then(Value::as_str).unwrap_or("").get(..10).unwrap_or("");
        let active = p.get("active").and_then(Value::as_bool).unwrap_or(false);
        out.push(format!("plan {plan}: {} {until}", if active { "paid until" } else { "ended" }));
    }
    for e in v.get("experts").and_then(Value::as_array).into_iter().flatten() {
        let s = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("");
        out.push(format!("  {:<22} {}", s("slug"), s("state")));
    }
    out
}

async fn send(req: reqwest::RequestBuilder, key: Option<String>) -> Result<(reqwest::StatusCode, String)> {
    let req = match key {
        Some(k) => req.bearer_auth(k),
        None => req,
    };
    let res = req.send().await.context("could not reach c0mpute.com")?;
    let status = res.status();
    Ok((status, res.text().await.unwrap_or_default()))
}

pub async fn run(cmd: Option<ExpertsCmd>) -> Result<()> {
    let http = client()?;
    match cmd.unwrap_or(ExpertsCmd::List { json: false, site: None }) {
        ExpertsCmd::List { json, site } => {
            let site = base_url(site.as_deref());
            let (status, text) = send(http.get(format!("{site}/api/v1/experts")), api_key()).await?;
            if !status.is_success() {
                return Err(api_error(status, &text));
            }
            if json {
                println!("{text}");
                return Ok(());
            }
            let v: Value = serde_json::from_str(&text).context("bad response")?;
            for line in list_lines(&v) {
                println!("{line}");
            }
            Ok(())
        }
        ExpertsCmd::Status { json, site } => {
            let site = base_url(site.as_deref());
            let (status, text) = send(http.get(format!("{site}/api/v1/experts/me")), Some(require_key()?)).await?;
            if !status.is_success() {
                return Err(api_error(status, &text));
            }
            if json {
                println!("{text}");
                return Ok(());
            }
            let v: Value = serde_json::from_str(&text).context("bad response")?;
            for line in status_lines(&v) {
                println!("{line}");
            }
            Ok(())
        }
        ExpertsCmd::Trial { site } => {
            let site = base_url(site.as_deref());
            let (status, text) = send(http.post(format!("{site}/api/v1/experts/trial")), Some(require_key()?)).await?;
            if !status.is_success() {
                return Err(api_error(status, &text));
            }
            let v: Value = serde_json::from_str(&text).context("bad response")?;
            let created = v.get("created").and_then(Value::as_bool).unwrap_or(false);
            let ends = v.pointer("/trial/ends_at").and_then(Value::as_str).unwrap_or("").get(..10).unwrap_or("");
            println!("{} ends {ends}", if created { "free trial started:" } else { "you already have a trial:" });
            Ok(())
        }
        ExpertsCmd::Subscribe { plan, email, site } => {
            let site = base_url(site.as_deref());
            let body = json!({ "plan": slug_of(&plan), "email": email });
            let (status, text) = send(http.post(format!("{site}/api/v1/experts/checkout")).json(&body), Some(require_key()?)).await?;
            if !status.is_success() {
                return Err(api_error(status, &text));
            }
            let v: Value = serde_json::from_str(&text).context("bad response")?;
            let url = v.get("url").and_then(Value::as_str).context("no checkout url in the response")?;
            println!("pay ${} with CoinPay: {url}", v.get("amount_usd").and_then(Value::as_f64).unwrap_or(0.0));
            Ok(())
        }
    }
}

pub async fn ask(a: AskArgs) -> Result<()> {
    let key = require_key()?;
    let mut question = a.question.join(" ");
    if question.trim().is_empty() || question.trim() == "-" {
        question.clear();
        std::io::stdin().read_to_string(&mut question).context("could not read the question from stdin")?;
    }
    if question.trim().is_empty() {
        return Err(anyhow!("usage: c0mpute ask <expert> \"question\" (see `c0mpute experts`)"));
    }
    let site = base_url(a.site.as_deref());
    let slug = slug_of(&a.expert);
    let http = client()?;
    let (status, text) = send(http.post(format!("{site}/api/v1/experts/{slug}/ask")).json(&ask_body(&question)).timeout(std::time::Duration::from_secs(1200)) /* the server streams underneath now; 320 s cut slow nodes off */, Some(key)).await?;
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !status.is_success() {
        let mut err = api_error(status, &text);
        if let Some(link) = v.pointer("/error/subscribe").or_else(|| v.pointer("/error/connect")).and_then(Value::as_str) {
            err = anyhow!("{err}\n{link}");
        }
        return Err(err);
    }
    if a.json {
        println!("{text}");
        return Ok(());
    }
    println!("{}", v.pointer("/choices/0/message/content").and_then(Value::as_str).unwrap_or("(empty answer)"));
    if let Some(d) = v.pointer("/x_c0mpute_expert/disclaimer").and_then(Value::as_str) {
        eprintln!("\n-- {d}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_accept_the_model_form() {
        assert_eq!(slug_of("expert:Tax-Info"), "tax-info");
        assert_eq!(slug_of(" security-analyst "), "security-analyst");
    }

    #[test]
    fn ask_body_is_one_user_turn() {
        assert_eq!(ask_body("hi"), json!({"messages":[{"role":"user","content":"hi"}]}));
    }

    #[test]
    fn lists_and_status_render() {
        let v = json!({"pricing":{"trial_days":90,"all_experts_usd":29,"period_days":30},"data":[{"slug":"security-analyst","category":"security","summary":"Triage CVEs","store":{"price_usd":9}}]});
        let l = list_lines(&v);
        assert!(l[0].starts_with("security-analyst"));
        assert!(l[0].contains("$9"));
        assert!(l.iter().any(|x| x.contains("90 days free")));
        let s = json!({"coinpay":true,"trial":{"started_at":"2026-11-22T12:00:00Z","ends_at":"2027-02-20T12:00:00Z","days_left":90},"subscriptions":[],"experts":[{"slug":"tax-info","state":"trial"}]});
        let st = status_lines(&s);
        assert!(st[0].contains("90 days left"));
        assert!(st[0].contains("2027-02-20"));
        assert!(status_lines(&json!({"coinpay":false}))[0].contains("not connected"));
    }
}
