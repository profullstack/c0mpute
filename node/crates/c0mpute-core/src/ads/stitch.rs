//! Podcast dynamic ad insertion: the publisher's opt-in, and splicing audio
//! creatives into an episode with ffmpeg.
//!
//! **Consent.** A show takes ads only if its own hosting site carries
//! [`OPT_IN_FILE`] with `enabled = true`. The site record is signed by the
//! show's owner and pins the manifest (and so this file) by hash, so the
//! opt-in is the publisher's signed statement; no gateway can add one. Shows
//! without it are always served clean. `c0mpute podcasting publish` writes it
//! from an `[ads]` table in `show.toml`.
//!
//! **Splicing.** ffmpeg decodes the episode and the creatives, resamples them
//! to the episode's rate and channel layout, joins them with the `concat`
//! filter (cutting the episode at the mid-roll point with `atrim`), and
//! re-encodes in the episode's own container and codec. Re-encoding is the
//! only join that works for any pair of inputs; stream-copy concat needs
//! identical codec parameters, which creatives from elsewhere never have.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::campaign::Placement;

/// The publisher's opt-in file, at the root of the show's hosting site.
pub const OPT_IN_FILE: &str = "c0mpute-ads.json";
pub const OPT_IN_KIND: &str = "c0mpute-podcast-ads";

/// Contents of [`OPT_IN_FILE`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShowAds {
    pub v: u8,
    pub kind: String,
    pub enabled: bool,
    /// Which breaks the show sells: podcast_pre / podcast_mid / podcast_post.
    pub placements: Vec<Placement>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Where the mid-roll goes; default half way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mid_at_secs: Option<u64>,
    /// Lowest CPM the show accepts, USD millionths.
    #[serde(default)]
    pub floor_cpm_micros: u64,
    /// Longest creative the show runs, seconds.
    #[serde(default = "default_max_ad_secs")]
    pub max_ad_secs: u64,
    /// Where the show's share of ad revenue goes, `<COIN>:<address>`
    /// ([`super::billing`]). Without one the show's impressions are not paid
    /// (and not charged to advertisers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payout: Option<String>,
}

fn default_max_ad_secs() -> u64 {
    60
}

impl ShowAds {
    pub fn new(placements: Vec<Placement>) -> Self {
        ShowAds {
            v: 1,
            kind: OPT_IN_KIND.into(),
            enabled: true,
            placements,
            categories: vec![],
            keywords: vec![],
            mid_at_secs: None,
            floor_cpm_micros: 0,
            max_ad_secs: default_max_ad_secs(),
            payout: None,
        }
    }

    /// Parse and check an opt-in file. Anything malformed means "no ads".
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let s: ShowAds = serde_json::from_slice(bytes).context("ads opt-in is not valid JSON")?;
        if s.v != 1 || s.kind != OPT_IN_KIND {
            bail!("not a c0mpute podcast ads opt-in");
        }
        if s.placements.is_empty() || !s.placements.iter().all(|p| p.is_podcast()) {
            bail!("a show sells podcast_pre, podcast_mid and/or podcast_post");
        }
        if let Some(p) = &s.payout
            && !super::billing::valid_payout(p)
        {
            bail!("ads payout must be <COIN>:<address>");
        }
        Ok(s)
    }

    pub fn allows(&self, p: Placement) -> bool {
        self.enabled && self.placements.contains(&p)
    }
}

/// An episode's audio stream, as ffprobe sees it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioInfo {
    pub duration_secs: Option<f64>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub bit_rate: Option<u64>,
}

/// ffprobe the first audio stream.
pub async fn probe(ffprobe: &Path, path: &Path) -> Option<AudioInfo> {
    let out = tokio::process::Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=sample_rate,channels,bit_rate:format=duration,bit_rate",
            "-of",
            "json",
        ])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_probe(&out.stdout)
}

pub fn parse_probe(json: &[u8]) -> Option<AudioInfo> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let num = |x: &serde_json::Value| -> Option<f64> {
        x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))
    };
    let stream = v["streams"].get(0)?;
    Some(AudioInfo {
        duration_secs: num(&v["format"]["duration"]).filter(|d| d.is_finite() && *d > 0.0),
        sample_rate: num(&stream["sample_rate"]).map(|x| x as u32).filter(|x| *x > 0),
        channels: stream["channels"].as_u64().map(|x| x as u32).filter(|x| *x > 0),
        bit_rate: num(&stream["bit_rate"])
            .or_else(|| num(&v["format"]["bit_rate"]))
            .map(|x| x as u64)
            .filter(|x| *x > 0),
    })
}

/// Encoder settings for an output extension: (ffmpeg args, MIME type).
pub fn encoder_for(ext: &str, bit_rate: Option<u64>) -> Option<(Vec<String>, &'static str)> {
    let kbps = bit_rate.map(|b| (b / 1000).clamp(48, 320)).unwrap_or(128);
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    Some(match ext.to_ascii_lowercase().as_str() {
        "mp3" => (s(&["-c:a", "libmp3lame", "-b:a", &format!("{kbps}k"), "-f", "mp3"]), "audio/mpeg"),
        "m4a" | "aac" => (
            s(&["-c:a", "aac", "-b:a", &format!("{kbps}k"), "-movflags", "+faststart", "-f", "ipod"]),
            "audio/x-m4a",
        ),
        "ogg" | "oga" => (s(&["-c:a", "libvorbis", "-q:a", "5", "-f", "ogg"]), "audio/ogg"),
        "opus" => (s(&["-c:a", "libopus", "-b:a", &format!("{}k", kbps.min(256)), "-f", "opus"]), "audio/opus"),
        "flac" => (s(&["-c:a", "flac", "-f", "flac"]), "audio/flac"),
        "wav" => (s(&["-c:a", "pcm_s16le", "-f", "wav"]), "audio/wav"),
        _ => return None,
    })
}

/// What to splice.
#[derive(Clone, Debug)]
pub struct StitchPlan {
    pub episode: PathBuf,
    pub pre: Option<PathBuf>,
    /// (creative, cut point in seconds).
    pub mid: Option<(PathBuf, f64)>,
    pub post: Option<PathBuf>,
    pub out: PathBuf,
    pub sample_rate: u32,
    pub channels: u32,
    pub bit_rate: Option<u64>,
}

impl StitchPlan {
    pub fn ad_count(&self) -> usize {
        self.pre.is_some() as usize + self.mid.is_some() as usize + self.post.is_some() as usize
    }
}

/// The ffmpeg argument list for a plan (without the program name).
pub fn ffmpeg_args(plan: &StitchPlan) -> Result<Vec<String>> {
    if plan.ad_count() == 0 {
        bail!("nothing to insert");
    }
    let ext = plan
        .out
        .extension()
        .and_then(|e| e.to_str())
        .context("output needs an extension")?;
    let (enc, _) = encoder_for(ext, plan.bit_rate).with_context(|| format!("cannot encode .{ext}"))?;
    let layout = if plan.channels == 1 { "mono" } else { "stereo" };
    let norm = format!("aresample={},aformat=sample_rates={}:channel_layouts={layout}", plan.sample_rate, plan.sample_rate);

    let mut args: Vec<String> = ["-hide_banner", "-nostdin", "-loglevel", "error", "-y"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut inputs: Vec<&Path> = vec![&plan.episode];
    let pre_i = plan.pre.as_ref().map(|p| {
        inputs.push(p);
        inputs.len() - 1
    });
    let mid_i = plan.mid.as_ref().map(|(p, _)| {
        inputs.push(p);
        inputs.len() - 1
    });
    let post_i = plan.post.as_ref().map(|p| {
        inputs.push(p);
        inputs.len() - 1
    });
    for i in &inputs {
        args.push("-i".into());
        args.push(i.to_string_lossy().into_owned());
    }

    let mut graph: Vec<String> = Vec::new();
    let mut order: Vec<String> = Vec::new();
    if let Some(i) = pre_i {
        graph.push(format!("[{i}:a:0]{norm}[pre]"));
        order.push("[pre]".into());
    }
    match &plan.mid {
        Some((_, at)) => {
            let at = format!("{:.3}", at.max(0.0));
            graph.push(format!(
                "[0:a:0]{norm},asplit=2[ea][eb];[ea]atrim=end={at},asetpts=PTS-STARTPTS[e1];[eb]atrim=start={at},asetpts=PTS-STARTPTS[e2]"
            ));
            graph.push(format!("[{}:a:0]{norm}[mid]", mid_i.unwrap_or(0)));
            order.extend(["[e1]".into(), "[mid]".into(), "[e2]".into()]);
        }
        None => {
            graph.push(format!("[0:a:0]{norm}[ep]"));
            order.push("[ep]".into());
        }
    }
    if let Some(i) = post_i {
        graph.push(format!("[{i}:a:0]{norm}[post]"));
        order.push("[post]".into());
    }
    graph.push(format!("{}concat=n={}:v=0:a=1[out]", order.join(""), order.len()));
    args.push("-filter_complex".into());
    args.push(graph.join(";"));
    args.extend(["-map".into(), "[out]".into(), "-vn".into(), "-map_metadata".into(), "0".into()]);
    args.extend(enc);
    args.push(plan.out.to_string_lossy().into_owned());
    Ok(args)
}

/// Run ffmpeg for a plan, killing it after `timeout`.
pub async fn run(ffmpeg: &Path, plan: &StitchPlan, timeout: Duration) -> Result<()> {
    let args = ffmpeg_args(plan)?;
    let child = tokio::process::Command::new(ffmpeg)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("start {}", ffmpeg.display()))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .context("ffmpeg timed out")??;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("ffmpeg failed: {}", err.lines().last().unwrap_or("").trim());
    }
    let len = std::fs::metadata(&plan.out).map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        bail!("ffmpeg wrote nothing");
    }
    Ok(())
}

/// A tool on PATH (or a configured path) that actually runs.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let p = crate::supervisor::which_ffmpeg()
        .and_then(|f| f.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file())
        .or_else(|| {
            std::env::var_os("PATH").and_then(|path| {
                std::env::split_paths(&path)
                    .map(|d| d.join(name))
                    .find(|p| p.is_file())
            })
        })?;
    let ok = std::process::Command::new(&p)
        .arg("-version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(pre: bool, mid: Option<f64>, post: bool, out: &str) -> StitchPlan {
        StitchPlan {
            episode: "/w/ep.mp3".into(),
            pre: pre.then(|| "/w/pre.mp3".into()),
            mid: mid.map(|t| (PathBuf::from("/w/mid.m4a"), t)),
            post: post.then(|| "/w/post.wav".into()),
            out: out.into(),
            sample_rate: 44100,
            channels: 2,
            bit_rate: Some(96_000),
        }
    }

    fn graph(args: &[String]) -> &str {
        let i = args.iter().position(|a| a == "-filter_complex").unwrap();
        &args[i + 1]
    }

    #[test]
    fn pre_and_post_concat_in_order() {
        let a = ffmpeg_args(&plan(true, None, true, "/w/out.mp3")).unwrap();
        let inputs: Vec<&String> = a.windows(2).filter(|w| w[0] == "-i").map(|w| &w[1]).collect();
        assert_eq!(inputs, ["/w/ep.mp3", "/w/pre.mp3", "/w/post.wav"]);
        let g = graph(&a);
        assert!(g.contains("[1:a:0]aresample=44100"), "{g}");
        assert!(g.ends_with("[pre][ep][post]concat=n=3:v=0:a=1[out]"), "{g}");
        assert!(a.windows(2).any(|w| w[0] == "-c:a" && w[1] == "libmp3lame"));
        assert!(a.windows(2).any(|w| w[0] == "-b:a" && w[1] == "96k"), "keeps the episode's bitrate");
        assert_eq!(a.last().unwrap(), "/w/out.mp3");
        assert!(a.contains(&"-nostdin".to_string()));
    }

    #[test]
    fn mid_roll_cuts_the_episode() {
        let a = ffmpeg_args(&plan(false, Some(754.25), false, "/w/out.m4a")).unwrap();
        let g = graph(&a);
        assert!(g.contains("asplit=2[ea][eb]"));
        assert!(g.contains("[ea]atrim=end=754.250"), "{g}");
        assert!(g.contains("[eb]atrim=start=754.250"), "{g}");
        assert!(g.contains("[1:a:0]aresample=44100,aformat=sample_rates=44100:channel_layouts=stereo[mid]"));
        assert!(g.ends_with("[e1][mid][e2]concat=n=3:v=0:a=1[out]"), "{g}");
        assert!(a.windows(2).any(|w| w[0] == "-c:a" && w[1] == "aac"));
        assert!(a.windows(2).any(|w| w[0] == "-f" && w[1] == "ipod"));
    }

    #[test]
    fn all_three_and_mono() {
        let mut p = plan(true, Some(10.0), true, "/w/out.ogg");
        p.channels = 1;
        let a = ffmpeg_args(&p).unwrap();
        let g = graph(&a);
        assert!(g.ends_with("[pre][e1][mid][e2][post]concat=n=5:v=0:a=1[out]"), "{g}");
        assert!(g.contains("channel_layouts=mono"));
        assert!(g.contains("[2:a:0]") && g.contains("[3:a:0]"));
    }

    #[test]
    fn refuses_nothing_to_do_and_unknown_formats() {
        assert!(ffmpeg_args(&plan(false, None, false, "/w/out.mp3")).is_err());
        assert!(ffmpeg_args(&plan(true, None, false, "/w/out.xyz")).is_err());
        for ext in ["mp3", "m4a", "aac", "ogg", "oga", "opus", "flac", "wav"] {
            assert!(encoder_for(ext, None).is_some(), "{ext}");
        }
    }

    #[test]
    fn opt_in_parsing() {
        let ok = serde_json::to_vec(&ShowAds::new(vec![Placement::PodcastPre, Placement::PodcastPost])).unwrap();
        let s = ShowAds::parse(&ok).unwrap();
        assert!(s.allows(Placement::PodcastPre));
        assert!(!s.allows(Placement::PodcastMid));
        let mut off = s.clone();
        off.enabled = false;
        assert!(!off.allows(Placement::PodcastPre));
        assert!(ShowAds::parse(b"{}").is_err());
        assert!(ShowAds::parse(b"not json").is_err());
        let ctv = serde_json::to_vec(&ShowAds::new(vec![Placement::CtvPreroll])).unwrap();
        assert!(ShowAds::parse(&ctv).is_err(), "podcast placements only");
        let mut wrong = ShowAds::new(vec![Placement::PodcastPre]);
        wrong.kind = "something-else".into();
        assert!(ShowAds::parse(&serde_json::to_vec(&wrong).unwrap()).is_err());
    }

    #[test]
    fn probe_json() {
        let j = br#"{"streams":[{"sample_rate":"48000","channels":1,"bit_rate":"64000"}],"format":{"duration":"12.5","bit_rate":"65000"}}"#;
        let i = parse_probe(j).unwrap();
        assert_eq!(i.sample_rate, Some(48000));
        assert_eq!(i.channels, Some(1));
        assert_eq!(i.bit_rate, Some(64000));
        assert_eq!(i.duration_secs, Some(12.5));
        assert!(parse_probe(br#"{"streams":[]}"#).is_none());
    }

    /// A real splice, when ffmpeg is installed: the output lasts as long as
    /// the episode plus the ads.
    #[tokio::test]
    async fn real_ffmpeg_stitch_when_available() {
        let (Some(ffmpeg), Some(ffprobe)) = (find_tool("ffmpeg"), find_tool("ffprobe")) else {
            eprintln!("skipping: ffmpeg/ffprobe not installed");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let tone = |name: &str, secs: u32, freq: u32, rate: u32, ch: u32| {
            let p = tmp.path().join(name);
            let ok = std::process::Command::new(&ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
                .arg(format!("sine=frequency={freq}:sample_rate={rate}:duration={secs}"))
                .args(["-ac", &ch.to_string()])
                .arg(&p)
                .status()
                .unwrap()
                .success();
            assert!(ok, "generate {name}");
            p
        };
        let ep = tone("ep.mp3", 20, 440, 44100, 2);
        let pre = tone("pre.wav", 3, 880, 22050, 1);
        let mid = tone("mid.m4a", 2, 660, 48000, 2);
        let post = tone("post.ogg", 4, 330, 16000, 1);
        let info = probe(&ffprobe, &ep).await.unwrap();
        let out = tmp.path().join("out.mp3");
        let plan = StitchPlan {
            episode: ep,
            pre: Some(pre),
            mid: Some((mid, 10.0)),
            post: Some(post),
            out: out.clone(),
            sample_rate: info.sample_rate.unwrap(),
            channels: info.channels.unwrap(),
            bit_rate: info.bit_rate,
        };
        run(&ffmpeg, &plan, Duration::from_secs(60)).await.unwrap();
        let got = probe(&ffprobe, &out).await.unwrap().duration_secs.unwrap();
        assert!((got - 29.0).abs() < 0.5, "20 s episode + 3 + 2 + 4 s of ads = {got}");
    }
}
