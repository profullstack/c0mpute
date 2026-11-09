//! The advertiser's side (`c0mpute ads ...`): turn media files into a
//! creative site, sign campaigns, keep a local copy, publish, and count
//! receipts.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use c0mpute_proto::Hash;
use serde::Deserialize;

use super::campaign::{self, CampaignRecord, Creative, CreativeKind, FreqCap, Placement, Schedule, Targeting};
use super::receipt::{ReceiptBatch, Report};
use super::{Ads, Options};
use crate::hosting::site::{self, SiteRecord};
use crate::hosting::{self, Hosting};

/// An ad-set descriptor an ad generator (or a person) can drop next to the
/// media: `ad-set.json` with an optional landing URL and name.
pub const AD_SET_FILE: &str = "ad-set.json";

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AdSetMeta {
    #[serde(default, alias = "landingUrl", alias = "url")]
    pub landing_url: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// Media found in the given files and folders, plus any `ad-set.json`.
pub struct CreativeInput {
    pub files: Vec<PathBuf>,
    pub meta: AdSetMeta,
}

/// Collect media from files and (non-recursively) folders, e.g. an
/// ad-generator-agent output folder: `*.mp3|m4a|wav|ogg|opus|flac` (audio),
/// `*.mp4|webm|mov` (video), `*.png|jpg|webp` (image), `*.gif`.
pub fn collect_creatives(paths: &[PathBuf]) -> Result<CreativeInput> {
    let mut files = Vec::new();
    let mut meta = AdSetMeta::default();
    for p in paths {
        if p.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(p)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|f| f.is_file() && media_kind(f).is_some())
                .collect();
            found.sort();
            files.extend(found);
            let m = p.join(AD_SET_FILE);
            if m.is_file() {
                meta = serde_json::from_slice(&std::fs::read(&m)?).with_context(|| format!("parse {}", m.display()))?;
            }
        } else if p.is_file() {
            if media_kind(p).is_none() {
                bail!("{} is not a supported creative (audio, video, image or gif)", p.display());
            }
            files.push(p.clone());
        } else {
            bail!("{} does not exist", p.display());
        }
    }
    if files.is_empty() {
        bail!("no creatives found (mp3/m4a/wav/ogg/opus/flac, mp4/webm/mov, png/jpg/webp, gif)");
    }
    Ok(CreativeInput { files, meta })
}

fn media_kind(p: &Path) -> Option<(CreativeKind, &'static str)> {
    CreativeKind::from_ext(p.extension()?.to_str()?)
}

/// Duration (ms), width and height from ffprobe, when it is installed.
pub fn probe_media(path: &Path) -> (Option<u64>, Option<u32>, Option<u32>) {
    let Ok(out) = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration:stream=width,height,codec_type",
            "-of",
            "json",
        ])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .output()
    else {
        return (None, None, None);
    };
    if !out.status.success() {
        return (None, None, None);
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let dur = v["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| (d * 1000.0).round() as u64);
    let video = v["streams"]
        .as_array()
        .and_then(|a| a.iter().find(|s| s["width"].as_u64().is_some()).cloned());
    let w = video.as_ref().and_then(|s| s["width"].as_u64()).map(|x| x as u32);
    let h = video.as_ref().and_then(|s| s["height"].as_u64()).map(|x| x as u32);
    (dur, w, h)
}

/// Copy the creatives into `out` (emptied first) as `creatives/<name>` and
/// describe each one. `durations` overrides probing (file name -> ms).
pub fn build_creative_dir(files: &[PathBuf], out: &Path, duration_override_ms: Option<u64>) -> Result<Vec<Creative>> {
    if out.exists() {
        std::fs::remove_dir_all(out).with_context(|| format!("clear {}", out.display()))?;
    }
    std::fs::create_dir_all(out.join("creatives"))?;
    let mut used = std::collections::HashSet::new();
    let mut creatives = Vec::new();
    let mut total = 0u64;
    for f in files {
        let (kind, mime) = media_kind(f).context("not a creative")?;
        let bytes = std::fs::read(f).with_context(|| format!("read {}", f.display()))?;
        total += bytes.len() as u64;
        if total > site::MAX_SITE_BYTES {
            bail!("creatives add up to more than {} MiB", site::MAX_SITE_BYTES / (1024 * 1024));
        }
        let stem: String = f
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("creative")
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '-' })
            .collect();
        let mut name = stem.clone();
        let mut n = 2;
        while !used.insert(name.clone()) {
            name = format!("{n}-{stem}");
            n += 1;
        }
        let rel = format!("creatives/{name}");
        std::fs::write(out.join(&rel), &bytes)?;
        let (dur, w, h) = match kind {
            CreativeKind::Audio | CreativeKind::Video => {
                let (d, w, h) = probe_media(f);
                (d.or(duration_override_ms), w, h)
            }
            _ => {
                let (_, w, h) = probe_media(f);
                (None, w, h)
            }
        };
        if matches!(kind, CreativeKind::Audio | CreativeKind::Video) && dur.is_none() {
            bail!(
                "could not read the duration of {} (install ffprobe, or pass --duration-secs)",
                f.display()
            );
        }
        creatives.push(Creative {
            hash: Hash::of(&bytes).to_hex(),
            path: rel,
            kind,
            mime: mime.to_string(),
            bytes: bytes.len() as u64,
            duration_ms: dur,
            width: w,
            height: h,
        });
    }
    Ok(creatives)
}

/// What the advertiser asks for (CLI flags).
#[derive(Clone, Debug, Default)]
pub struct CampaignSpec {
    pub name: String,
    pub placements: Vec<Placement>,
    pub categories: Vec<String>,
    pub keywords: Vec<String>,
    pub geo: Vec<String>,
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub freq_cap: Option<FreqCap>,
    pub max_cpm_micros: u64,
    pub landing_url: String,
}

/// Build (unsigned) a campaign record from a spec and its creatives.
pub fn record_from_spec(spec: &CampaignSpec, owner_hex: &str, creatives: Vec<Creative>, seq: u64) -> CampaignRecord {
    CampaignRecord {
        v: 1,
        campaign_id: campaign::campaign_id_for(owner_hex, &spec.name),
        name: spec.name.clone(),
        owner: owner_hex.to_string(),
        seq,
        paused: false,
        creative_site: site::site_id_for(owner_hex, &campaign::creative_site_name(&spec.name)),
        creatives,
        targeting: Targeting {
            placements: spec.placements.clone(),
            categories: spec.categories.clone(),
            keywords: spec.keywords.clone(),
            geo: spec.geo.clone(),
        },
        schedule: Schedule {
            start: spec.start,
            end: spec.end,
        },
        freq_cap: spec.freq_cap,
        max_cpm_micros: spec.max_cpm_micros,
        landing_url: spec.landing_url.clone(),
        sig: String::new(),
    }
}

/// Milliseconds since the epoch: the CLI's version numbers.
pub fn now_seq() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `YYYY-MM-DD`, RFC 3339 or unix seconds -> unix seconds.
pub fn parse_time(s: &str) -> Result<i64> {
    if let Ok(n) = s.trim().parse::<i64>() {
        return Ok(n);
    }
    crate::podcast::dates::parse_date_input(s).with_context(|| format!("not a date: {s:?} (use YYYY-MM-DD or RFC 3339)"))
}

// ── local store ──────────────────────────────────────────────────────────

pub fn ads_dir() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ads")
}

/// `<data_dir>/ads/campaigns/<name>/`: `campaign.json` (the signed record)
/// and `site/` (the creative site as last published).
pub fn campaign_dir(name: &str) -> PathBuf {
    ads_dir().join("campaigns").join(name)
}

pub fn save_local(rec: &CampaignRecord) -> Result<()> {
    let d = campaign_dir(&rec.name);
    std::fs::create_dir_all(&d)?;
    std::fs::write(d.join("campaign.json"), serde_json::to_vec_pretty(rec)?)?;
    Ok(())
}

pub fn load_local(name_or_id: &str) -> Result<CampaignRecord> {
    let direct = campaign_dir(name_or_id).join("campaign.json");
    if direct.is_file() {
        return Ok(serde_json::from_slice(&std::fs::read(&direct)?)?);
    }
    list_local()?
        .into_iter()
        .find(|r| r.campaign_id == name_or_id || r.campaign_id.starts_with(name_or_id))
        .with_context(|| format!("no local campaign {name_or_id:?} (c0mpute ads campaign ls)"))
}

pub fn list_local() -> Result<Vec<CampaignRecord>> {
    let root = ads_dir().join("campaigns");
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.flatten() {
            if let Ok(b) = std::fs::read(e.path().join("campaign.json"))
                && let Ok(r) = serde_json::from_slice::<CampaignRecord>(&b)
            {
                out.push(r);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

// ── publishing ───────────────────────────────────────────────────────────

/// Publish a creative folder as the campaign's hosting site and the signed
/// campaign, on already-running services. Returns the site record.
pub async fn publish_with(
    hosting: &Hosting,
    ads: &Ads,
    keypair: &c0mpute_net::libp2p::identity::Keypair,
    creative_dir: Option<&Path>,
    record: &CampaignRecord,
) -> Result<Option<SiteRecord>> {
    let site_rec = match creative_dir {
        Some(dir) => {
            let name = campaign::creative_site_name(&record.name);
            let built = site::build_from_dir(dir, &name)?;
            let srec = SiteRecord::sign(
                keypair,
                &name,
                &Hash::of(&built.manifest.to_bytes()),
                now_seq(),
                built.manifest.total_bytes(),
                vec![],
            )?;
            if srec.site_id != record.creative_site {
                bail!("creative site id does not match the campaign");
            }
            for c in &record.creatives {
                let f = built.manifest.files.get(&c.path).with_context(|| format!("{} missing from creative site", c.path))?;
                if f.size != c.bytes {
                    bail!("{} changed since the campaign was built", c.path);
                }
            }
            hosting.publish_site(&built, srec.clone()).await?;
            Some(srec)
        }
        None => None,
    };
    ads.publish_campaign(record.clone()).await?;
    Ok(site_rec)
}

/// `c0mpute ads campaign create|update|pause`: boot a buyer node, publish,
/// and stay up until the creative site has network copies (or `wait`).
pub async fn publish(record: &CampaignRecord, creative_dir: Option<&Path>, wait: Duration) -> Result<usize> {
    let keypair = hosting::publisher_keypair()?;
    let supervisor = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    let h = Hosting::start(supervisor.libp2p.clone(), supervisor.store.clone(), hosting::Options::default()).await?;
    let ads = Ads::start(supervisor.libp2p.clone(), h.clone(), Options::default()).await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let site_rec = publish_with(&h, &ads, &keypair, creative_dir, record).await?;
    eprintln!("published campaign {} ({}) version {}", record.name, record.campaign_id, record.seq);
    let started = Instant::now();
    let mut copies = 0;
    // Campaign-only changes (pause, bid): gossip a few times and go.
    let wait = if site_rec.is_some() { wait } else { wait.min(Duration::from_secs(20)) };
    while started.elapsed() < wait {
        tokio::time::sleep(Duration::from_secs(5)).await;
        ads.regossip(record).await;
        if let Some(s) = &site_rec {
            h.regossip(s).await;
            let n = h.holders(&s.site_id).await.len();
            if n != copies {
                copies = n;
                eprintln!("  creatives: {copies}/{} network copies", hosting::TARGET_REPLICAS);
            }
            if copies >= hosting::TARGET_REPLICAS {
                break;
            }
        }
    }
    Ok(copies)
}

/// Gather receipts for a campaign: ask over gossip (every gateway answers
/// with its own chain) and, optionally, straight from gateway URLs.
pub async fn collect_report(ads: &Ads, campaign_id: &str, gateways: &[String], wait: Duration) -> Report {
    ads.request_report(campaign_id).await;
    let mut batches: Vec<ReceiptBatch> = Vec::new();
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build();
    if let Ok(client) = http {
        for g in gateways {
            let url = format!("{}/ads/v1/receipts?campaign={campaign_id}", g.trim_end_matches('/'));
            match client.get(&url).send().await {
                Ok(r) => match r.json::<serde_json::Value>().await {
                    Ok(v) => {
                        if let Ok(b) = serde_json::from_value::<Vec<ReceiptBatch>>(v["batches"].clone()) {
                            batches.extend(b);
                        }
                    }
                    Err(e) => eprintln!("  {g}: {e}"),
                },
                Err(e) => eprintln!("  {g}: {e}"),
            }
        }
    }
    let started = Instant::now();
    let mut asked = Instant::now();
    while started.elapsed() < wait {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if asked.elapsed() > Duration::from_secs(5) {
            ads.request_report(campaign_id).await;
            asked = Instant::now();
        }
    }
    batches.extend(ads.collected(campaign_id));
    Report::build(campaign_id, &batches)
}

/// `c0mpute ads report`.
pub async fn report(campaign_id: &str, gateways: &[String], wait: Duration) -> Result<Report> {
    let supervisor = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    let h = Hosting::start(supervisor.libp2p.clone(), supervisor.store.clone(), hosting::Options::default()).await?;
    let ads = Ads::start(supervisor.libp2p.clone(), h, Options::default()).await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    Ok(collect_report(&ads, campaign_id, gateways, wait).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_media_and_ad_set_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("adset");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("banner_300x250.png"), b"png").unwrap();
        std::fs::write(d.join("preroll.gif"), b"gif").unwrap();
        std::fs::write(d.join("copy.txt"), b"not media").unwrap();
        std::fs::write(d.join(AD_SET_FILE), br#"{"landingUrl":"https://example.org/","name":"spring"}"#).unwrap();
        let input = collect_creatives(&[d.clone()]).unwrap();
        assert_eq!(input.files.len(), 2);
        assert_eq!(input.meta.landing_url.as_deref(), Some("https://example.org/"));
        assert!(collect_creatives(&[d.join("copy.txt")]).is_err());
        assert!(collect_creatives(&[tmp.path().join("missing")]).is_err());

        let out = tmp.path().join("site");
        let cs = build_creative_dir(&input.files, &out, None).unwrap();
        assert_eq!(cs.len(), 2);
        assert!(out.join("creatives/banner_300x250.png").is_file());
        assert_eq!(cs[0].hash, Hash::of(b"png").to_hex());
        assert_eq!(cs[1].kind, CreativeKind::Gif);
        // Audio without a readable duration needs an override.
        std::fs::write(d.join("spot.mp3"), b"not really mp3").unwrap();
        let input = collect_creatives(&[d.clone()]).unwrap();
        assert!(build_creative_dir(&input.files, &out, None).is_err());
        let cs = build_creative_dir(&input.files, &out, Some(5000)).unwrap();
        assert_eq!(cs.iter().find(|c| c.kind == CreativeKind::Audio).unwrap().duration_ms, Some(5000));
    }

    #[test]
    fn spec_to_signed_record() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let owner = site::owner_hex(&kp).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("b.png"), b"png").unwrap();
        let cs = build_creative_dir(&[tmp.path().join("b.png")], &tmp.path().join("s"), None).unwrap();
        let spec = CampaignSpec {
            name: "display-test".into(),
            placements: vec![Placement::Display],
            max_cpm_micros: 1_000_000,
            landing_url: "https://example.org/".into(),
            ..Default::default()
        };
        let rec = record_from_spec(&spec, &owner, cs, 1).sign(&kp).unwrap();
        assert!(rec.verify());
        assert_eq!(rec.creative_site, site::site_id_for(&owner, "ads-display-test"));
        assert_eq!(parse_time("2026-06-01").unwrap(), 1_780_272_000);
        assert_eq!(parse_time("1700000000").unwrap(), 1_700_000_000);
        assert!(parse_time("someday").is_err());
    }
}
