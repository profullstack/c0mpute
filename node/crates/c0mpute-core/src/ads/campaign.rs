//! What a campaign is: the advertiser's signed statement "campaign `name`
//! runs these creatives, for these placements and audiences, at most this
//! CPM, until then".
//!
//! Like a hosting [`SiteRecord`](crate::hosting::site::SiteRecord), a record
//! is verifiable from its bytes alone: the campaign id is derived from the
//! owner key and the name, so only the owner can publish a new version, and a
//! higher `seq` replaces a lower one. Creatives are pinned by blake3 hash and
//! live in a hosting site (three self-healing copies, fetched with failover),
//! so no ad server holds them.

use anyhow::{Context, Result, bail};
use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};

const SIGNING_DOMAIN: &str = "c0mpute-ads-campaign-v1";
/// Most creatives in one campaign.
pub const MAX_CREATIVES: usize = 32;
/// Most categories, keywords or countries in one targeting list.
pub const MAX_TARGETS: usize = 64;
/// A signed record has to fit one gossip message with room to spare.
pub const MAX_RECORD_BYTES: usize = 32 * 1024;
/// USD millionths in a dollar (`max_cpm_micros`).
pub const MICROS: u64 = 1_000_000;

/// Where an ad runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// Before a podcast episode (dynamic ad insertion).
    PodcastPre,
    /// Inside a podcast episode.
    PodcastMid,
    /// After a podcast episode.
    PodcastPost,
    /// Connected TV / web video, before the content (VAST).
    CtvPreroll,
    CtvMidroll,
    CtvPostroll,
    /// A display slot (image or GIF).
    Display,
}

impl Placement {
    pub const ALL: [Placement; 7] = [
        Placement::PodcastPre,
        Placement::PodcastMid,
        Placement::PodcastPost,
        Placement::CtvPreroll,
        Placement::CtvMidroll,
        Placement::CtvPostroll,
        Placement::Display,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Placement::PodcastPre => "podcast_pre",
            Placement::PodcastMid => "podcast_mid",
            Placement::PodcastPost => "podcast_post",
            Placement::CtvPreroll => "ctv_preroll",
            Placement::CtvMidroll => "ctv_midroll",
            Placement::CtvPostroll => "ctv_postroll",
            Placement::Display => "display",
        }
    }

    /// Accepts the canonical names plus short forms (`pre`, `preroll`, ...).
    pub fn parse(s: &str) -> Option<Placement> {
        let s = s.trim().to_ascii_lowercase().replace('-', "_");
        Some(match s.as_str() {
            "podcast_pre" | "pre" => Placement::PodcastPre,
            "podcast_mid" | "mid" => Placement::PodcastMid,
            "podcast_post" | "post" => Placement::PodcastPost,
            "ctv_preroll" | "preroll" | "ctv" => Placement::CtvPreroll,
            "ctv_midroll" | "midroll" => Placement::CtvMidroll,
            "ctv_postroll" | "postroll" => Placement::CtvPostroll,
            "display" => Placement::Display,
            _ => return None,
        })
    }

    pub fn is_podcast(&self) -> bool {
        matches!(self, Placement::PodcastPre | Placement::PodcastMid | Placement::PodcastPost)
    }

    pub fn is_ctv(&self) -> bool {
        matches!(self, Placement::CtvPreroll | Placement::CtvMidroll | Placement::CtvPostroll)
    }

    /// Whether a creative of `kind` can fill this placement.
    pub fn accepts(&self, kind: CreativeKind) -> bool {
        match self {
            p if p.is_podcast() => kind == CreativeKind::Audio,
            p if p.is_ctv() => kind == CreativeKind::Video,
            _ => matches!(kind, CreativeKind::Image | CreativeKind::Gif),
        }
    }
}

impl std::fmt::Display for Placement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreativeKind {
    Audio,
    Video,
    Image,
    Gif,
}

impl CreativeKind {
    /// From a file extension: the media an ad set is made of.
    pub fn from_ext(ext: &str) -> Option<(CreativeKind, &'static str)> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "mp3" => (CreativeKind::Audio, "audio/mpeg"),
            "m4a" => (CreativeKind::Audio, "audio/mp4"),
            "aac" => (CreativeKind::Audio, "audio/aac"),
            "ogg" | "oga" => (CreativeKind::Audio, "audio/ogg"),
            "opus" => (CreativeKind::Audio, "audio/opus"),
            "wav" => (CreativeKind::Audio, "audio/wav"),
            "flac" => (CreativeKind::Audio, "audio/flac"),
            "mp4" | "m4v" => (CreativeKind::Video, "video/mp4"),
            "webm" => (CreativeKind::Video, "video/webm"),
            "mov" => (CreativeKind::Video, "video/quicktime"),
            "png" => (CreativeKind::Image, "image/png"),
            "jpg" | "jpeg" => (CreativeKind::Image, "image/jpeg"),
            "webp" => (CreativeKind::Image, "image/webp"),
            "gif" => (CreativeKind::Gif, "image/gif"),
            _ => return None,
        })
    }
}

/// One creative: a file in the campaign's creative site, pinned by hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Creative {
    /// blake3 (hex) of the file's bytes.
    pub hash: String,
    /// Path inside the creative site.
    pub path: String,
    pub kind: CreativeKind,
    pub mime: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// Who and where.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Targeting {
    pub placements: Vec<Placement>,
    /// Lowercase. Empty = any category.
    #[serde(default)]
    pub categories: Vec<String>,
    /// Lowercase. Empty = any keywords.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// ISO 3166-1 alpha-2, uppercase. Empty = anywhere.
    #[serde(default)]
    pub geo: Vec<String>,
}

/// Unix seconds; either end open.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,
}

impl Schedule {
    pub fn contains(&self, now: i64) -> bool {
        self.start.is_none_or(|s| now >= s) && self.end.is_none_or(|e| now < e)
    }
}

/// "At most `max` impressions per listener per `per_secs`". A hint each
/// gateway enforces for the listeners it serves; there is no global count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreqCap {
    pub max: u32,
    pub per_secs: u64,
}

/// The advertiser's signed campaign.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CampaignRecord {
    pub v: u8,
    pub campaign_id: String,
    pub name: String,
    /// Advertiser's ed25519 public key, hex.
    pub owner: String,
    /// Version: higher wins. The CLI uses milliseconds since the epoch.
    pub seq: u64,
    #[serde(default)]
    pub paused: bool,
    /// Hosting site (same owner) that holds the creatives.
    pub creative_site: String,
    pub creatives: Vec<Creative>,
    pub targeting: Targeting,
    #[serde(default)]
    pub schedule: Schedule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freq_cap: Option<FreqCap>,
    /// Highest price per thousand impressions, USD millionths.
    pub max_cpm_micros: u64,
    pub landing_url: String,
    /// ed25519 signature (hex) over [`CampaignRecord::signing_bytes`].
    #[serde(default)]
    pub sig: String,
}

/// The id a campaign with this owner and name always has (20 hex chars).
pub fn campaign_id_for(owner_hex: &str, name: &str) -> String {
    Hash::of(format!("c0mpute-ad-campaign\n{owner_hex}\n{name}").as_bytes()).to_hex()[..20].to_string()
}

/// "2.50" -> 2_500_000 USD millionths.
pub fn parse_usd_micros(s: &str) -> Result<u64> {
    let s = s.trim().trim_start_matches('$');
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if whole.is_empty() && frac.is_empty() {
        bail!("not an amount: {s:?}");
    }
    if frac.len() > 6 || !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        bail!("not an amount in USD (up to 6 decimals): {s:?}");
    }
    let w: u64 = if whole.is_empty() { 0 } else { whole.parse()? };
    let f: u64 = if frac.is_empty() { 0 } else { format!("{frac:0<6}").parse()? };
    w.checked_mul(MICROS)
        .and_then(|x| x.checked_add(f))
        .context("amount too large")
}

pub fn fmt_usd(micros: u64) -> String {
    format!("${}.{:02}", micros / MICROS, (micros % MICROS) / 10_000)
}

fn valid_country(c: &str) -> bool {
    c.len() == 2 && c.chars().all(|ch| ch.is_ascii_uppercase())
}

fn valid_term(t: &str) -> bool {
    !t.is_empty() && t.len() <= 64 && t == t.to_lowercase() && !t.chars().any(|c| c.is_control() || c == ',')
}

/// Normalise a user-supplied term list (split on commas, trim, lowercase,
/// dedupe, keep order).
pub fn norm_terms<I: IntoIterator<Item = S>, S: AsRef<str>>(items: I) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        for t in item.as_ref().split(',') {
            let t = t.trim().to_lowercase();
            if !t.is_empty() && !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

pub fn norm_countries<I: IntoIterator<Item = S>, S: AsRef<str>>(items: I) -> Vec<String> {
    norm_terms(items).into_iter().map(|c| c.to_ascii_uppercase()).collect()
}

impl CampaignRecord {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.sig = String::new();
        let mut out = format!("{SIGNING_DOMAIN}\n").into_bytes();
        out.extend(serde_json::to_vec(&unsigned).expect("campaign serializes"));
        out
    }

    /// Fill in id/owner, check the fields, and sign with the owner's key.
    pub fn sign(mut self, keypair: &c0mpute_net::libp2p::identity::Keypair) -> Result<Self> {
        let owner = crate::hosting::site::owner_hex(keypair).context("ads need an ed25519 identity")?;
        self.v = 1;
        self.campaign_id = campaign_id_for(&owner, &self.name);
        self.owner = owner;
        self.sig = String::new();
        self.check_fields()?;
        let sig = keypair
            .sign(&self.signing_bytes())
            .map_err(|e| anyhow::anyhow!("sign campaign: {e}"))?;
        self.sig = hex::encode(sig);
        Ok(self)
    }

    pub fn check_fields(&self) -> Result<()> {
        if self.v != 1 {
            bail!("unsupported campaign version {}", self.v);
        }
        if !crate::hosting::site::valid_name(&self.name) {
            bail!("campaign name must be 1-63 of a-z, 0-9 and '-'");
        }
        if self.campaign_id != campaign_id_for(&self.owner, &self.name) {
            bail!("campaign id does not match owner and name");
        }
        if self.creative_site != crate::hosting::site::site_id_for(&self.owner, &creative_site_name(&self.name)) {
            bail!("creatives must live in the owner's own creative site");
        }
        if self.creatives.is_empty() || self.creatives.len() > MAX_CREATIVES {
            bail!("a campaign needs 1-{MAX_CREATIVES} creatives");
        }
        for c in &self.creatives {
            if Hash::from_hex(&c.hash).is_err() || !crate::hosting::site::valid_path(&c.path) {
                bail!("creative {:?} has a bad hash or path", c.path);
            }
            if c.mime.is_empty() || c.mime.len() > 64 || c.mime.chars().any(|ch| ch.is_control() || ch == '"') {
                bail!("creative {:?} has a bad MIME type", c.path);
            }
            if matches!(c.kind, CreativeKind::Audio | CreativeKind::Video) && c.duration_ms.is_none_or(|d| d == 0) {
                bail!("audio/video creative {:?} needs a duration", c.path);
            }
        }
        let t = &self.targeting;
        if t.placements.is_empty() {
            bail!("pick at least one placement");
        }
        for p in &t.placements {
            if !self.creatives.iter().any(|c| p.accepts(c.kind)) {
                bail!("placement {p} has no creative it can run (podcast needs audio, ctv video, display image/gif)");
            }
        }
        if t.categories.len() > MAX_TARGETS || t.keywords.len() > MAX_TARGETS || t.geo.len() > MAX_TARGETS {
            bail!("at most {MAX_TARGETS} categories, keywords or countries");
        }
        if !t.categories.iter().chain(&t.keywords).all(|x| valid_term(x)) {
            bail!("categories and keywords are lowercase, 1-64 chars, no commas");
        }
        if !t.geo.iter().all(|c| valid_country(c)) {
            bail!("geo is a list of ISO 3166-1 alpha-2 codes (US, DE, ...)");
        }
        if let (Some(s), Some(e)) = (self.schedule.start, self.schedule.end)
            && e <= s
        {
            bail!("schedule ends before it starts");
        }
        if let Some(f) = self.freq_cap
            && (f.max == 0 || f.per_secs == 0)
        {
            bail!("frequency cap needs max >= 1 and a period");
        }
        if self.max_cpm_micros == 0 {
            bail!("max CPM must be above zero");
        }
        let u = url::Url::parse(&self.landing_url).context("landing URL")?;
        if !matches!(u.scheme(), "http" | "https") || self.landing_url.len() > 2048 {
            bail!("landing URL must be http(s)");
        }
        if serde_json::to_vec(self).map(|b| b.len()).unwrap_or(usize::MAX) > MAX_RECORD_BYTES {
            bail!("campaign record is over {MAX_RECORD_BYTES} bytes");
        }
        Ok(())
    }

    /// Fields sane, ids match, and the owner signed it.
    pub fn verify(&self) -> bool {
        if self.check_fields().is_err() {
            return false;
        }
        crate::ads::verify_sig(&self.owner, &self.signing_bytes(), &self.sig)
    }

    pub fn creative(&self, hash: &str) -> Option<&Creative> {
        self.creatives.iter().find(|c| c.hash == hash)
    }
}

/// The hosting site name that holds a campaign's creatives.
pub fn creative_site_name(campaign_name: &str) -> String {
    // Site names allow 63 chars; campaign names too, so trim from the end.
    let n = format!("ads-{campaign_name}");
    n.chars().take(63).collect::<String>().trim_end_matches('-').to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn sample(keypair: &c0mpute_net::libp2p::identity::Keypair, name: &str, seq: u64) -> CampaignRecord {
        let owner = crate::hosting::site::owner_hex(keypair).unwrap();
        CampaignRecord {
            v: 1,
            campaign_id: String::new(),
            name: name.into(),
            owner: owner.clone(),
            seq,
            paused: false,
            creative_site: crate::hosting::site::site_id_for(&owner, &creative_site_name(name)),
            creatives: vec![
                Creative {
                    hash: Hash::of(b"audio").to_hex(),
                    path: "spot.mp3".into(),
                    kind: CreativeKind::Audio,
                    mime: "audio/mpeg".into(),
                    bytes: 5,
                    duration_ms: Some(5000),
                    width: None,
                    height: None,
                },
                Creative {
                    hash: Hash::of(b"video").to_hex(),
                    path: "preroll.mp4".into(),
                    kind: CreativeKind::Video,
                    mime: "video/mp4".into(),
                    bytes: 5,
                    duration_ms: Some(5000),
                    width: Some(1280),
                    height: Some(720),
                },
            ],
            targeting: Targeting {
                placements: vec![Placement::PodcastPre, Placement::CtvPreroll],
                ..Default::default()
            },
            schedule: Schedule::default(),
            freq_cap: None,
            max_cpm_micros: 2 * MICROS,
            landing_url: "https://example.org/".into(),
            sig: String::new(),
        }
    }

    #[test]
    fn sign_and_verify() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let rec = sample(&kp, "spring-sale", 1).sign(&kp).unwrap();
        assert!(rec.verify());
        assert_eq!(rec.campaign_id, campaign_id_for(&rec.owner, "spring-sale"));
        assert_eq!(rec.campaign_id.len(), 20);

        // Any change breaks the signature.
        let mut bid = rec.clone();
        bid.max_cpm_micros += 1;
        assert!(!bid.verify());
        let mut paused = rec.clone();
        paused.paused = true;
        assert!(!paused.verify());

        // Someone else cannot publish under this campaign id.
        let other = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let mut forged = rec.clone();
        forged.owner = crate::hosting::site::owner_hex(&other).unwrap();
        forged.sig = hex::encode(other.sign(&forged.signing_bytes()).unwrap());
        assert!(!forged.verify(), "id no longer matches owner + name");

        // Round-trips through JSON (the gossip encoding) and still verifies.
        let back: CampaignRecord = serde_json::from_slice(&serde_json::to_vec(&rec).unwrap()).unwrap();
        assert!(back.verify());
    }

    #[test]
    fn refuses_bad_fields() {
        let kp = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let mut r = sample(&kp, "x", 1);
        r.targeting.placements = vec![Placement::Display];
        assert!(r.sign(&kp).is_err(), "display needs an image creative");
        let mut r = sample(&kp, "x", 1);
        r.targeting.geo = vec!["usa".into()];
        assert!(r.sign(&kp).is_err());
        let mut r = sample(&kp, "x", 1);
        r.landing_url = "javascript:alert(1)".into();
        assert!(r.sign(&kp).is_err());
        let mut r = sample(&kp, "x", 1);
        r.schedule = Schedule { start: Some(10), end: Some(5) };
        assert!(r.sign(&kp).is_err());
        let mut r = sample(&kp, "x", 1);
        r.creatives[0].duration_ms = None;
        assert!(r.sign(&kp).is_err());
        let mut r = sample(&kp, "x", 1);
        r.creative_site = "0123456789abcdef0123".into();
        assert!(r.sign(&kp).is_err(), "creatives must be in the owner's site");
        let mut r = sample(&kp, "Bad Name", 1);
        r.creative_site = String::new();
        assert!(r.sign(&kp).is_err());
    }

    #[test]
    fn money_and_terms() {
        assert_eq!(parse_usd_micros("2.5").unwrap(), 2_500_000);
        assert_eq!(parse_usd_micros("$0.000001").unwrap(), 1);
        assert_eq!(parse_usd_micros("12").unwrap(), 12_000_000);
        assert!(parse_usd_micros("1.0000001").is_err());
        assert!(parse_usd_micros("-1").is_err());
        assert!(parse_usd_micros("abc").is_err());
        assert_eq!(fmt_usd(2_500_000), "$2.50");
        assert_eq!(norm_terms(["Tech, rust", "tech", " AI "]), vec!["tech", "rust", "ai"]);
        assert_eq!(norm_countries(["us,de", "US"]), vec!["US", "DE"]);
        assert_eq!(Placement::parse("pre"), Some(Placement::PodcastPre));
        assert_eq!(Placement::parse("ctv-preroll"), Some(Placement::CtvPreroll));
        assert_eq!(Placement::parse("nope"), None);
        for p in Placement::ALL {
            assert_eq!(Placement::parse(p.as_str()), Some(p));
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(json, format!("\"{}\"", p.as_str()));
        }
    }
}
