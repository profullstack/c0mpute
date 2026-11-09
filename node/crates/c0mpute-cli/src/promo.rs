//! `c0mpute promo <url>`: a 20-40 s cartoon promo video of a website, made on
//! the c0mpute network through c0mpute.com (`POST /api/v1/promo`, then polls).
//! Infernet reads the site's branding, writes a storyboard, draws keyframes,
//! narrates with Kokoro and scores with ACE-Step on c0mpute workers, and cuts
//! the MP4. Needs `c0mpute chat login` (or `C0MPUTE_API_KEY`).
//!
//! ```text
//! c0mpute promo https://c0mpute.com                    # Pixar-style, 16:9
//! c0mpute promo example.com --style anime --vertical   # 9:16 for phones
//! c0mpute promo example.com -o launch.mp4 --brief "for GPU owners"
//! c0mpute promo example.com --no-wait --json           # just start it
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Value, json};

use crate::ai_chat::{api_error, base_url, client, require_key};

pub const STYLES: [&str; 7] = ["pixar", "anime", "claymation", "flat", "watercolor", "comic", "cinematic"];
const STAGES: [&str; 7] = ["brand", "script", "frames", "motion", "voice", "music", "render"];

#[derive(Args, Debug, Clone)]
pub struct PromoArgs {
    /// The website to promote (public http/https; a bare domain works).
    pub url: String,
    /// Visual style: pixar, anime, claymation, flat, watercolor, comic, cinematic.
    #[arg(long, short, default_value = "pixar")]
    pub style: String,
    /// 9:16 vertical video for phones instead of 16:9.
    #[arg(long)]
    pub vertical: bool,
    /// Direction for the storyboard: audience, message, tone.
    #[arg(long)]
    pub brief: Option<String>,
    /// Where to save the MP4 (default: <host>-promo.mp4). The .srt goes next to it.
    #[arg(long, short)]
    pub out: Option<PathBuf>,
    /// Start the job and print its id without waiting.
    #[arg(long)]
    pub no_wait: bool,
    /// Print JSON (the start response, or the final status) instead of progress lines.
    #[arg(long)]
    pub json: bool,
    /// c0mpute site URL (default https://c0mpute.com, or $C0MPUTE_URL).
    #[arg(long)]
    pub site: Option<String>,
}

/// `example.com/x` -> `example-com`, for the default file name.
pub fn default_out(url: &str) -> PathBuf {
    let host = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split(['/', '?', '#', ':'])
        .next()
        .unwrap_or("site")
        .trim_start_matches("www.");
    let slug: String = host
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let slug = slug.trim_matches('-');
    PathBuf::from(format!("{}-promo.mp4", if slug.is_empty() { "site" } else { slug }))
}

/// One line per stage that changed, e.g. `frames: 3/6`.
pub fn stage_lines(status: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for k in STAGES {
        if let Some(s) = status.pointer(&format!("/stages/{k}")) {
            let state = s.get("state").and_then(Value::as_str).unwrap_or("pending");
            let detail = s.get("detail").and_then(Value::as_str).unwrap_or("");
            out.push((k.to_string(), format!("{state}{}{detail}", if detail.is_empty() { "" } else { ": " })));
        }
    }
    out
}

pub async fn run(a: PromoArgs) -> Result<()> {
    let style = a.style.to_ascii_lowercase();
    if !STYLES.contains(&style.as_str()) {
        bail!("--style must be one of: {}", STYLES.join(", "));
    }
    let site = base_url(a.site.as_deref());
    let key = require_key()?;
    let http = client()?;
    let mut body = json!({ "url": a.url, "style": style, "vertical": a.vertical });
    if let Some(b) = &a.brief {
        body["brief"] = json!(b);
    }
    let res = http
        .post(format!("{site}/api/v1/promo"))
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("could not reach {site}"))?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if status.as_u16() != 202 {
        if status.as_u16() == 401 {
            bail!("{}\nrun `c0mpute chat login`", api_error(status, &text));
        }
        return Err(api_error(status, &text));
    }
    let start: Value = serde_json::from_str(&text).context("bad start response")?;
    let id = start.get("id").and_then(Value::as_str).context("no job id in the response")?.to_string();
    if a.no_wait {
        if a.json {
            println!("{text}");
        } else {
            println!("{id}\npoll: {site}/api/v1/promo/{id}");
        }
        return Ok(());
    }
    if !a.json {
        eprintln!("promo {id}: {style}{} video of {}", if a.vertical { " vertical" } else { "" }, a.url);
    }

    let started = Instant::now();
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut failures = 0;
    let done: Value = loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if started.elapsed() > Duration::from_secs(60 * 60) {
            bail!("the promo did not finish within an hour (job {id})");
        }
        let res = match http.get(format!("{site}/api/v1/promo/{id}")).bearer_auth(&key).send().await {
            Ok(r) => r,
            Err(e) => {
                failures += 1;
                if failures > 12 {
                    bail!("lost contact with {site}: {e}");
                }
                continue;
            }
        };
        let code = res.status();
        let text = res.text().await.unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if code.as_u16() == 503 || (code.is_server_error() && v.get("status").is_none()) {
            failures += 1;
            if failures > 12 {
                return Err(api_error(code, &text));
            }
            continue;
        }
        failures = 0;
        if !code.is_success() {
            return Err(api_error(code, &text));
        }
        if !a.json {
            for line in stage_lines(&v) {
                if !seen.contains(&line) {
                    eprintln!("  [{:>4}s] {:<7} {}", started.elapsed().as_secs(), line.0, line.1);
                    seen.retain(|(k, _)| k != &line.0);
                    seen.push(line);
                }
            }
        }
        if code.as_u16() == 200 {
            break v;
        }
    };

    let out = a.out.clone().unwrap_or_else(|| default_out(&a.url));
    let video = http
        .get(format!("{site}/api/v1/promo/{id}/video"))
        .bearer_auth(&key)
        .send()
        .await
        .with_context(|| format!("could not download the video from {site}"))?;
    if !video.status().is_success() {
        let s = video.status();
        return Err(api_error(s, &video.text().await.unwrap_or_default()));
    }
    let bytes = video.bytes().await.context("video download was cut off")?;
    std::fs::write(&out, &bytes).with_context(|| format!("could not write {}", out.display()))?;
    let srt_path = out.with_extension("srt");
    if let Ok(r) = http.get(format!("{site}/api/v1/promo/{id}/captions")).bearer_auth(&key).send().await {
        if r.status().is_success() {
            if let Ok(t) = r.text().await {
                let _ = std::fs::write(&srt_path, t);
            }
        }
    }
    if a.json {
        let mut v = done.clone();
        v["file"] = json!(out.display().to_string());
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        let dur = done.get("duration").and_then(Value::as_f64).unwrap_or(0.0);
        let motion = match done.get("motion").and_then(Value::as_str) {
            Some("kenburns") => "keyframes with camera moves",
            Some(_) => "img2vid motion",
            None => "",
        };
        println!(
            "{} ({:.0}s, {:.1} MB, {motion}) in {}s",
            out.display(),
            dur,
            bytes.len() as f64 / 1e6,
            started.elapsed().as_secs()
        );
        if srt_path.exists() {
            println!("{}", srt_path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_out_slugs_the_host() {
        assert_eq!(default_out("https://www.c0mpute.com/chat?x=1"), PathBuf::from("c0mpute-com-promo.mp4"));
        assert_eq!(default_out("example.io"), PathBuf::from("example-io-promo.mp4"));
        assert_eq!(default_out("http://a.b:8080/"), PathBuf::from("a-b-promo.mp4"));
    }

    #[test]
    fn stage_lines_follow_the_pipeline_order() {
        let v = json!({ "stages": {
            "render": { "state": "pending" },
            "brand": { "state": "done", "detail": "c0mpute" },
            "frames": { "state": "running", "detail": "2/6" }
        }});
        let lines = stage_lines(&v);
        assert_eq!(lines[0], ("brand".into(), "done: c0mpute".into()));
        assert_eq!(lines[1], ("frames".into(), "running: 2/6".into()));
        assert_eq!(lines[2], ("render".into(), "pending".into()));
    }
}
