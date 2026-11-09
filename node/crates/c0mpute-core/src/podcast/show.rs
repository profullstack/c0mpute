//! `podcasting`: turn a folder of audio into a podcast site (RSS 2.0 feed
//! with iTunes + Podcasting 2.0 tags, an episode page, the audio itself) and
//! publish it with hosting, so the show lives on three self-healing copies
//! and every gateway serves it at `/s/<site_id>/feed.xml`.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::dates;
use crate::hosting::site::{self, MAX_SITE_BYTES, SiteRecord};

/// The Podcasting 2.0 namespace UUID for `podcast:guid` (uuid v5).
pub const PODCAST_GUID_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0xead4c236_bf58_58c6_a2c6_a6b28d128cb6);

/// Audio extensions a show may carry, with the enclosure MIME type.
pub const AUDIO_TYPES: &[(&str, &str)] = &[
    ("mp3", "audio/mpeg"),
    ("m4a", "audio/x-m4a"),
    ("aac", "audio/aac"),
    ("ogg", "audio/ogg"),
    ("oga", "audio/ogg"),
    ("opus", "audio/opus"),
    ("flac", "audio/flac"),
    ("wav", "audio/wav"),
];

pub fn audio_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    AUDIO_TYPES.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m)
}

/// `show.toml` (every field optional; flags override).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShowMeta {
    pub title: Option<String>,
    pub author: Option<String>,
    pub description: Option<String>,
    pub language: Option<String>,
    /// Cover art file, relative to the show folder.
    pub image: Option<PathBuf>,
    pub category: Option<String>,
    pub explicit: Option<bool>,
    pub link: Option<String>,
    /// Owner email (`itunes:owner`, `podcast:locked owner=`).
    pub email: Option<String>,
    /// `podcast:locked`: default yes (other hosts should refuse to import it).
    pub locked: Option<bool>,
    /// Public URL the site is reachable at (for absolute enclosure URLs).
    pub base_url: Option<String>,
    pub value: Option<ValueMeta>,
}

/// A `podcast:value` block. Written only when an address is given.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValueMeta {
    #[serde(rename = "type", default = "lightning")]
    pub kind: String,
    #[serde(default = "keysend")]
    pub method: String,
    pub address: String,
    pub name: Option<String>,
    #[serde(default = "node")]
    pub recipient_type: String,
    pub suggested: Option<String>,
}

fn lightning() -> String {
    "lightning".into()
}
fn keysend() -> String {
    "keysend".into()
}
fn node() -> String {
    "node".into()
}

/// One `[[episode]]` in `episodes.toml`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpisodeMeta {
    pub file: PathBuf,
    pub title: Option<String>,
    pub description: Option<String>,
    /// `YYYY-MM-DD`, RFC 3339 or RFC 2822. Default: the file's mtime.
    pub pub_date: Option<String>,
    pub guid: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub episode_type: Option<String>,
    pub explicit: Option<bool>,
    /// Seconds. Default: ffprobe, when it is installed.
    pub duration: Option<u64>,
    pub link: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EpisodesFile {
    #[serde(default, rename = "episode")]
    episodes: Vec<EpisodeMeta>,
}

/// Everything a show is built from.
#[derive(Clone, Debug, Default)]
pub struct ShowInput {
    pub show: ShowMeta,
    /// Source audio file and its metadata.
    pub episodes: Vec<(PathBuf, EpisodeMeta)>,
    /// Cover art source file.
    pub image: Option<PathBuf>,
}

/// Read a show folder (`show.toml`, `episodes.toml`, every audio file in it)
/// or a list of audio files.
pub fn load_input(paths: &[PathBuf]) -> Result<ShowInput> {
    if paths.is_empty() {
        bail!("give a show folder or one or more audio files");
    }
    let mut input = ShowInput::default();
    if paths.len() == 1 && paths[0].is_dir() {
        let dir = &paths[0];
        let show_toml = dir.join("show.toml");
        if show_toml.exists() {
            let text = std::fs::read_to_string(&show_toml)?;
            input.show = toml::from_str(&text).with_context(|| format!("parse {}", show_toml.display()))?;
        }
        let ep_toml = dir.join("episodes.toml");
        let listed: Vec<EpisodeMeta> = if ep_toml.exists() {
            let text = std::fs::read_to_string(&ep_toml)?;
            toml::from_str::<EpisodesFile>(&text)
                .with_context(|| format!("parse {}", ep_toml.display()))?
                .episodes
        } else {
            vec![]
        };
        for meta in listed {
            let src = dir.join(&meta.file);
            if !src.is_file() {
                bail!("episodes.toml lists {} but it is not a file", src.display());
            }
            if audio_mime(&src).is_none() {
                bail!("{} is not a supported audio type (mp3, m4a, aac, ogg, opus, flac, wav)", src.display());
            }
            input.episodes.push((src, meta));
        }
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && audio_mime(p).is_some())
            .collect();
        found.sort();
        for p in found {
            let canon = std::fs::canonicalize(&p).ok();
            let already = input
                .episodes
                .iter()
                .any(|(s, _)| std::fs::canonicalize(s).ok() == canon);
            if !already {
                let meta = EpisodeMeta {
                    file: p.file_name().map(PathBuf::from).unwrap_or_default(),
                    ..Default::default()
                };
                input.episodes.push((p, meta));
            }
        }
        input.image = match &input.show.image {
            Some(rel) => Some(dir.join(rel)),
            None => ["cover.jpg", "cover.jpeg", "cover.png", "artwork.jpg", "artwork.png"]
                .iter()
                .map(|n| dir.join(n))
                .find(|p| p.is_file()),
        };
    } else {
        for p in paths {
            if !p.is_file() || audio_mime(p).is_none() {
                bail!("{} is not an audio file (mp3, m4a, aac, ogg, opus, flac, wav)", p.display());
            }
            input.episodes.push((
                p.clone(),
                EpisodeMeta {
                    file: p.file_name().map(PathBuf::from).unwrap_or_default(),
                    ..Default::default()
                },
            ));
        }
    }
    if input.episodes.is_empty() {
        bail!("no audio files found (mp3, m4a, aac, ogg, opus, flac, wav)");
    }
    Ok(input)
}

/// One episode as written into the site.
#[derive(Clone, Debug)]
pub struct BuiltEpisode {
    pub title: String,
    pub description: String,
    pub guid: String,
    /// Path inside the site, e.g. `episodes/ep-1.mp3`.
    pub path: String,
    /// Enclosure URL as written in the feed.
    pub url: String,
    pub bytes: u64,
    pub mime: &'static str,
    pub published: i64,
    pub duration_secs: Option<u64>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub episode_type: String,
    pub explicit: bool,
    pub link: Option<String>,
}

/// A show written to a folder, ready for `hosting`.
#[derive(Clone, Debug)]
pub struct BuiltShow {
    pub title: String,
    pub podcast_guid: String,
    pub feed_xml: String,
    pub episodes: Vec<BuiltEpisode>,
    pub base_url: Option<String>,
    pub total_bytes: u64,
}

/// `podcast:guid` for a c0mpute show: uuid v5 (Podcasting 2.0 namespace) of
/// `c0mpute.site/<site_id>`. Stable for the life of the site (owner + name).
pub fn podcast_guid_for(site_id: &str) -> String {
    uuid::Uuid::new_v5(&PODCAST_GUID_NAMESPACE, format!("c0mpute.site/{site_id}").as_bytes()).to_string()
}

/// XML/HTML text and attribute escaping.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if (c as u32) < 0x20 && c != '\n' && c != '\t' && c != '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// A file name safe in a URL path: `[A-Za-z0-9._-]`, others become `-`.
fn safe_file_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '-' })
        .collect();
    let s = s.trim_matches(|c| c == '-' || c == '.').to_string();
    if s.is_empty() { "episode".into() } else { s }
}

fn normalize_base(b: &str) -> Result<String> {
    let u = url::Url::parse(b.trim()).with_context(|| format!("base URL {b:?}"))?;
    if !matches!(u.scheme(), "http" | "https") {
        bail!("base URL must be http(s): {b}");
    }
    let mut s = u.to_string();
    if !s.ends_with('/') {
        s.push('/');
    }
    Ok(s)
}

/// `ffprobe`'s duration for a file, when ffprobe is installed.
pub fn probe_duration(path: &Path) -> Option<u64> {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let secs: f64 = s.trim().parse().ok()?;
    (secs.is_finite() && secs > 0.0).then(|| secs.round() as u64)
}

fn mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn place(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_file(dest)?;
    }
    if std::fs::hard_link(src, dest).is_err() {
        std::fs::copy(src, dest).with_context(|| format!("copy {}", src.display()))?;
    }
    Ok(())
}

/// Write the show into `out` (emptied first): `feed.xml`, `index.html`,
/// `episodes/*`, and `cover.*` when there is art. `site_id` is the hosting
/// site the show will be published as (it seeds `podcast:guid` and guids).
pub fn build_show(input: &ShowInput, name: &str, site_id: &str, base_url: Option<&str>, out: &Path) -> Result<BuiltShow> {
    if !site::valid_name(name) {
        bail!("show name must be 1-63 of a-z, 0-9 and '-' (it is the hosting site name)");
    }
    let base = base_url
        .or(input.show.base_url.as_deref())
        .map(normalize_base)
        .transpose()?;
    let url_for = |rel: &str| match &base {
        Some(b) => format!("{b}{rel}"),
        None => rel.to_string(),
    };
    let mut total: u64 = 0;
    for (src, _) in &input.episodes {
        total += std::fs::metadata(src).with_context(|| format!("stat {}", src.display()))?.len();
    }
    if let Some(img) = &input.image {
        total += std::fs::metadata(img).with_context(|| format!("cover art {}", img.display()))?.len();
    }
    if total > MAX_SITE_BYTES {
        bail!(
            "the show is {} MiB; a hosted site holds at most {} MiB (publish fewer or smaller episodes)",
            total / (1024 * 1024),
            MAX_SITE_BYTES / (1024 * 1024)
        );
    }
    if out.exists() {
        std::fs::remove_dir_all(out).with_context(|| format!("clear {}", out.display()))?;
    }
    std::fs::create_dir_all(out.join("episodes"))?;

    let show = &input.show;
    let title = show.title.clone().unwrap_or_else(|| name.to_string());
    let explicit = show.explicit.unwrap_or(false);
    let mut used = std::collections::HashSet::new();
    let mut episodes = Vec::new();
    for (src, meta) in &input.episodes {
        let mime = audio_mime(src).context("not an audio file")?;
        let file = src.file_name().and_then(|f| f.to_str()).unwrap_or("episode");
        let mut fname = safe_file_name(file);
        let mut n = 2;
        while !used.insert(fname.clone()) {
            let p = Path::new(&fname);
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("episode").to_string();
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("").to_string();
            fname = format!("{stem}-{n}.{ext}");
            n += 1;
        }
        let rel = format!("episodes/{fname}");
        place(src, &out.join(&rel))?;
        let bytes = std::fs::metadata(out.join(&rel))?.len();
        let published = match &meta.pub_date {
            Some(d) => dates::parse_date_input(d).with_context(|| format!("pub_date {d:?} for {}", src.display()))?,
            None => mtime(src),
        };
        let stem = src
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Episode")
            .replace(['_', '-'], " ");
        episodes.push(BuiltEpisode {
            title: meta.title.clone().unwrap_or(stem),
            description: meta.description.clone().unwrap_or_default(),
            guid: meta.guid.clone().unwrap_or_else(|| format!("c0mpute:{site_id}:{fname}")),
            url: url_for(&rel),
            path: rel,
            bytes,
            mime,
            published,
            duration_secs: meta.duration.or_else(|| probe_duration(src)),
            season: meta.season,
            episode: meta.episode,
            episode_type: meta.episode_type.clone().unwrap_or_else(|| "full".into()),
            explicit: meta.explicit.unwrap_or(explicit),
            link: meta.link.clone(),
        });
    }
    episodes.sort_by(|a, b| b.published.cmp(&a.published).then(a.path.cmp(&b.path)));

    let cover = match &input.image {
        Some(img) => {
            let ext = img
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .unwrap_or_else(|| "jpg".into());
            if !matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp") {
                bail!("cover art must be jpg, png or webp: {}", img.display());
            }
            let rel = format!("cover.{ext}");
            place(img, &out.join(&rel))?;
            Some(rel)
        }
        None => None,
    };

    let podcast_guid = podcast_guid_for(site_id);
    let built = BuiltShow {
        title,
        podcast_guid,
        feed_xml: String::new(),
        episodes,
        base_url: base.clone(),
        total_bytes: total,
    };
    let feed_xml = render_feed(show, &built, cover.as_deref().map(url_for).as_deref(), now_unix());
    std::fs::write(out.join("feed.xml"), &feed_xml)?;
    std::fs::write(out.join("index.html"), render_index(show, &built, cover.as_deref()))?;
    Ok(BuiltShow { feed_xml, ..built })
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The RSS 2.0 + iTunes + Podcasting 2.0 feed.
pub fn render_feed(show: &ShowMeta, b: &BuiltShow, cover_url: Option<&str>, now: i64) -> String {
    use std::fmt::Write;
    let mut x = String::new();
    let link = show
        .link
        .clone()
        .or_else(|| b.base_url.clone())
        .unwrap_or_else(|| "./".into());
    let description = show.description.clone().unwrap_or_else(|| b.title.clone());
    let explicit = show.explicit.unwrap_or(false);
    x.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    x.push_str(concat!(
        "<rss version=\"2.0\"",
        " xmlns:itunes=\"http://www.itunes.com/dtds/podcast-1.0.dtd\"",
        " xmlns:podcast=\"https://podcastindex.org/namespace/1.0\"",
        " xmlns:atom=\"http://www.w3.org/2005/Atom\"",
        " xmlns:content=\"http://purl.org/rss/1.0/modules/content/\">\n"
    ));
    x.push_str("<channel>\n");
    let _ = writeln!(x, "  <title>{}</title>", esc(&b.title));
    let _ = writeln!(x, "  <link>{}</link>", esc(&link));
    let _ = writeln!(x, "  <description>{}</description>", esc(&description));
    let _ = writeln!(x, "  <language>{}</language>", esc(show.language.as_deref().unwrap_or("en")));
    let _ = writeln!(x, "  <generator>c0mpute podcasting</generator>");
    let _ = writeln!(x, "  <lastBuildDate>{}</lastBuildDate>", dates::fmt_rfc2822(now));
    if let Some(base) = &b.base_url {
        let _ = writeln!(
            x,
            "  <atom:link href=\"{}feed.xml\" rel=\"self\" type=\"application/rss+xml\"/>",
            esc(base)
        );
    }
    if let Some(a) = &show.author {
        let _ = writeln!(x, "  <itunes:author>{}</itunes:author>", esc(a));
    }
    let _ = writeln!(x, "  <itunes:summary>{}</itunes:summary>", esc(&description));
    let _ = writeln!(x, "  <itunes:type>episodic</itunes:type>");
    let _ = writeln!(x, "  <itunes:explicit>{explicit}</itunes:explicit>");
    if let Some(email) = &show.email {
        let _ = writeln!(
            x,
            "  <itunes:owner><itunes:name>{}</itunes:name><itunes:email>{}</itunes:email></itunes:owner>",
            esc(show.author.as_deref().unwrap_or(&b.title)),
            esc(email)
        );
    }
    if let Some(c) = cover_url {
        let _ = writeln!(x, "  <itunes:image href=\"{}\"/>", esc(c));
        let _ = writeln!(
            x,
            "  <image><url>{}</url><title>{}</title><link>{}</link></image>",
            esc(c),
            esc(&b.title),
            esc(&link)
        );
    }
    if let Some(cat) = &show.category {
        let _ = writeln!(x, "  <itunes:category text=\"{}\"/>", esc(cat));
    }
    let _ = writeln!(x, "  <podcast:guid>{}</podcast:guid>", b.podcast_guid);
    let locked = if show.locked.unwrap_or(true) { "yes" } else { "no" };
    match &show.email {
        Some(e) => {
            let _ = writeln!(x, "  <podcast:locked owner=\"{}\">{locked}</podcast:locked>", esc(e));
        }
        None => {
            let _ = writeln!(x, "  <podcast:locked>{locked}</podcast:locked>");
        }
    }
    if let Some(v) = &show.value {
        let suggested = v
            .suggested
            .as_ref()
            .map(|s| format!(" suggested=\"{}\"", esc(s)))
            .unwrap_or_default();
        let _ = writeln!(
            x,
            "  <podcast:value type=\"{}\" method=\"{}\"{suggested}>\n    <podcast:valueRecipient name=\"{}\" type=\"{}\" address=\"{}\" split=\"100\"/>\n  </podcast:value>",
            esc(&v.kind),
            esc(&v.method),
            esc(v.name.as_deref().or(show.author.as_deref()).unwrap_or(&b.title)),
            esc(&v.recipient_type),
            esc(&v.address)
        );
    }
    for e in &b.episodes {
        x.push_str("  <item>\n");
        let _ = writeln!(x, "    <title>{}</title>", esc(&e.title));
        if !e.description.is_empty() {
            let _ = writeln!(x, "    <description>{}</description>", esc(&e.description));
        }
        let _ = writeln!(x, "    <guid isPermaLink=\"false\">{}</guid>", esc(&e.guid));
        let _ = writeln!(x, "    <pubDate>{}</pubDate>", dates::fmt_rfc2822(e.published));
        let _ = writeln!(
            x,
            "    <enclosure url=\"{}\" length=\"{}\" type=\"{}\"/>",
            esc(&e.url),
            e.bytes,
            e.mime
        );
        if let Some(l) = &e.link {
            let _ = writeln!(x, "    <link>{}</link>", esc(l));
        }
        if let Some(d) = e.duration_secs {
            let _ = writeln!(x, "    <itunes:duration>{}</itunes:duration>", dates::fmt_duration(d));
        }
        let _ = writeln!(x, "    <itunes:episodeType>{}</itunes:episodeType>", esc(&e.episode_type));
        if let Some(s) = e.season {
            let _ = writeln!(x, "    <itunes:season>{s}</itunes:season>");
        }
        if let Some(n) = e.episode {
            let _ = writeln!(x, "    <itunes:episode>{n}</itunes:episode>");
        }
        let _ = writeln!(x, "    <itunes:explicit>{}</itunes:explicit>", e.explicit);
        x.push_str("  </item>\n");
    }
    x.push_str("</channel>\n</rss>\n");
    x
}

/// A plain episode page with a player per episode. Paths are relative, so it
/// works under `/s/<site_id>/` and on a custom domain alike.
pub fn render_index(show: &ShowMeta, b: &BuiltShow, cover: Option<&str>) -> String {
    use std::fmt::Write;
    let mut h = String::new();
    let _ = write!(
        h,
        "<!doctype html>\n<html lang=\"{lang}\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{t}</title>\
<link rel=\"alternate\" type=\"application/rss+xml\" title=\"{t}\" href=\"feed.xml\">\
<style>:root{{color-scheme:light dark}}body{{font:16px/1.5 system-ui,sans-serif;max-width:46rem;margin:0 auto;padding:1rem}}\
img.cover{{max-width:12rem;border-radius:.5rem}}article{{border-top:1px solid #8884;padding:1rem 0}}audio{{width:100%}}\
.meta{{opacity:.7;font-size:.9em}}</style></head><body>\n",
        lang = esc(show.language.as_deref().unwrap_or("en")),
        t = esc(&b.title)
    );
    if let Some(c) = cover {
        let _ = writeln!(h, "<img class=\"cover\" src=\"{}\" alt=\"\">", esc(c));
    }
    let _ = writeln!(h, "<h1>{}</h1>", esc(&b.title));
    if let Some(a) = &show.author {
        let _ = writeln!(h, "<p class=\"meta\">by {}</p>", esc(a));
    }
    if let Some(d) = &show.description {
        let _ = writeln!(h, "<p>{}</p>", esc(d));
    }
    let _ = writeln!(
        h,
        "<p><a href=\"feed.xml\">RSS feed</a> (paste it into any podcast app, or <code>c0mpute podcasts subscribe</code>)</p>"
    );
    for e in &b.episodes {
        let _ = writeln!(h, "<article><h2>{}</h2>", esc(&e.title));
        let mut meta = dates::fmt_rfc2822(e.published)[..16].to_string();
        if let Some(d) = e.duration_secs {
            meta.push_str(&format!(" · {}", dates::fmt_duration(d)));
        }
        let _ = writeln!(h, "<p class=\"meta\">{}</p>", esc(&meta));
        if !e.description.is_empty() {
            let _ = writeln!(h, "<p>{}</p>", esc(&e.description));
        }
        let _ = writeln!(
            h,
            "<audio controls preload=\"none\" src=\"{}\"></audio>\n<p class=\"meta\"><a href=\"{}\" download>download</a> ({} MB)</p></article>",
            esc(&e.path),
            esc(&e.path),
            (e.bytes as f64 / 1_000_000.0 * 10.0).round() / 10.0
        );
    }
    h.push_str("</body></html>\n");
    h
}

/// Where a show is built before publishing: `<data_dir>/podcasting/<name>/site`.
pub fn build_dir(name: &str) -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("podcasting")
        .join(name)
        .join("site")
}

/// `c0mpute podcasting publish`: build the show and publish it as a hosting
/// site (a new version each time), then wait for network copies.
pub async fn publish(
    input: &ShowInput,
    name: &str,
    domains: Vec<String>,
    base_url: Option<String>,
    wait: Duration,
) -> Result<(SiteRecord, usize, BuiltShow)> {
    let keypair = crate::hosting::publisher_keypair()?;
    let owner = site::owner_hex(&keypair).context("hosting needs an ed25519 identity")?;
    let site_id = site::site_id_for(&owner, name);
    let base = base_url.or_else(|| input.show.base_url.clone()).or_else(|| domains.first().map(|d| format!("https://{d}/")));
    let out = build_dir(name);
    let built = build_show(input, name, &site_id, base.as_deref(), &out)?;
    let (record, copies) = crate::hosting::publish_dir(&out, name, domains, wait).await?;
    debug_assert_eq!(record.site_id, site_id);
    Ok((record, copies, built))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::podcast::rss;

    fn fixture(dir: &Path) {
        std::fs::write(dir.join("01 Pilot & Intro.mp3"), vec![1u8; 3000]).unwrap();
        std::fs::write(dir.join("ep2.m4a"), vec![2u8; 5000]).unwrap();
        std::fs::write(dir.join("notes.txt"), b"not audio").unwrap();
        std::fs::write(dir.join("cover.png"), b"\x89PNG fake").unwrap();
        std::fs::write(
            dir.join("show.toml"),
            r#"
title = "Tea & <Code>"
author = "Ann O'Nym"
description = "Short talks about \"tea\" and code."
category = "Technology"
email = "ann@example.org"
[value]
address = "02abcdef"
suggested = "0.00000005000"
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("episodes.toml"),
            r#"
[[episode]]
file = "01 Pilot & Intro.mp3"
title = "Pilot"
description = "Where it starts"
pub_date = "2026-06-01"
episode = 1
season = 1
duration = 61

[[episode]]
file = "ep2.m4a"
title = "Second"
pub_date = "2026-06-08T09:30:00Z"
episode = 2
"#,
        )
        .unwrap();
    }

    #[test]
    fn builds_a_valid_feed_that_parses_back() {
        let src = tempfile::tempdir().unwrap();
        fixture(src.path());
        let out = tempfile::tempdir().unwrap();
        let out = out.path().join("site");
        let input = load_input(&[src.path().to_path_buf()]).unwrap();
        assert_eq!(input.episodes.len(), 2, "notes.txt is not an episode");
        let site_id = "0123456789abcdef0123";
        let b = build_show(&input, "tea-code", site_id, Some("https://pod.example.org"), &out).unwrap();

        let f = rss::parse(&b.feed_xml).unwrap();
        assert_eq!(f.title, "Tea & <Code>");
        assert_eq!(f.author.as_deref(), Some("Ann O'Nym"));
        assert_eq!(f.description, "Short talks about \"tea\" and code.");
        assert_eq!(f.podcast_guid.as_deref(), Some(podcast_guid_for(site_id).as_str()));
        assert_eq!(f.locked, Some(true));
        assert_eq!(f.image.as_deref(), Some("https://pod.example.org/cover.png"));
        assert_eq!(f.self_url.as_deref(), Some("https://pod.example.org/feed.xml"));
        assert_eq!(f.explicit, Some(false));
        assert_eq!(f.episodes.len(), 2);
        // Newest first.
        assert_eq!(f.episodes[0].title, "Second");
        assert_eq!(f.episodes[1].title, "Pilot");
        for (ep, built) in f.episodes.iter().zip(&b.episodes) {
            let enc = ep.enclosure.as_ref().unwrap();
            let rel = enc.url.strip_prefix("https://pod.example.org/").unwrap();
            let on_disk = std::fs::metadata(out.join(rel)).unwrap().len();
            assert_eq!(enc.length, Some(on_disk), "enclosure length matches the file");
            assert_eq!(on_disk, built.bytes);
            assert!(ep.guid.as_deref().unwrap().starts_with("c0mpute:0123456789abcdef0123:"));
            assert!(ep.published.is_some(), "pubDate parses");
        }
        assert_eq!(f.episodes[0].enclosure.as_ref().unwrap().mime.as_deref(), Some("audio/x-m4a"));
        assert_eq!(f.episodes[1].enclosure.as_ref().unwrap().mime.as_deref(), Some("audio/mpeg"));
        assert_eq!(f.episodes[1].duration_secs, Some(61));
        assert_eq!(f.episodes[1].episode, Some(1));
        assert_eq!(f.episodes[0].published, dates::parse_date_input("2026-06-08T09:30:00Z"));
        // The odd file name became URL-safe.
        assert!(b.episodes[1].path == "episodes/01-Pilot---Intro.mp3", "{}", b.episodes[1].path);
        // Required channel tags are present in the raw XML.
        for tag in [
            "<rss version=\"2.0\"",
            "<podcast:locked owner=\"ann@example.org\">yes</podcast:locked>",
            "<podcast:value type=\"lightning\" method=\"keysend\" suggested=\"0.00000005000\">",
            "<podcast:valueRecipient name=\"Ann O&apos;Nym\" type=\"node\" address=\"02abcdef\" split=\"100\"/>",
            "<itunes:category text=\"Technology\"/>",
            "<itunes:owner>",
            "<itunes:duration>00:01:01</itunes:duration>",
            "<language>en</language>",
        ] {
            assert!(b.feed_xml.contains(tag), "missing {tag}");
        }
        let index = std::fs::read_to_string(out.join("index.html")).unwrap();
        assert!(index.contains("Tea &amp; &lt;Code&gt;"));
        assert!(index.contains("src=\"episodes/ep2.m4a\""));
        assert!(out.join("cover.png").is_file());
        assert!(!out.join("notes.txt").exists());
        // The built folder is a valid hosting site whose feed serves as RSS.
        let site = site::build_from_dir(&out, "tea-code").unwrap();
        assert_eq!(site.manifest.files["feed.xml"].content_type, "application/rss+xml");
        assert_eq!(site.manifest.files["episodes/ep2.m4a"].content_type, "audio/x-m4a");
    }

    #[test]
    fn no_value_block_unless_given_and_relative_urls_without_a_base() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.mp3"), vec![0u8; 10]).unwrap();
        let out = tempfile::tempdir().unwrap();
        let input = load_input(&[src.path().join("a.mp3")]).unwrap();
        let b = build_show(&input, "plain", "aaaaaaaaaaaaaaaaaaaa", None, &out.path().join("s")).unwrap();
        assert!(!b.feed_xml.contains("podcast:value"));
        assert!(!b.feed_xml.contains("atom:link"));
        let f = rss::parse(&b.feed_xml).unwrap();
        assert_eq!(f.title, "plain");
        assert_eq!(f.episodes[0].enclosure.as_ref().unwrap().url, "episodes/a.mp3");
        assert_eq!(f.episodes[0].enclosure.as_ref().unwrap().length, Some(10));
        assert_eq!(f.episodes[0].title, "a");
    }

    #[test]
    fn refuses_bad_input() {
        let src = tempfile::tempdir().unwrap();
        assert!(load_input(&[src.path().to_path_buf()]).is_err(), "empty folder");
        std::fs::write(src.path().join("x.txt"), b"x").unwrap();
        assert!(load_input(&[src.path().join("x.txt")]).is_err());
        std::fs::write(src.path().join("a.mp3"), b"x").unwrap();
        std::fs::write(src.path().join("show.toml"), "titel = \"typo\"").unwrap();
        assert!(load_input(&[src.path().to_path_buf()]).is_err(), "unknown show.toml key");
        let input = load_input(&[src.path().join("a.mp3")]).unwrap();
        let out = tempfile::tempdir().unwrap();
        assert!(build_show(&input, "Bad Name", "a", None, out.path()).is_err());
    }

    #[test]
    fn podcast_guid_is_stable_uuid_v5() {
        let g = podcast_guid_for("0123456789abcdef0123");
        assert_eq!(g, podcast_guid_for("0123456789abcdef0123"));
        assert_ne!(g, podcast_guid_for("0123456789abcdef0124"));
        let u = uuid::Uuid::parse_str(&g).unwrap();
        assert_eq!(u.get_version_num(), 5);
    }
}
