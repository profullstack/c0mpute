//! A small, forgiving RSS 2.0 reader for podcast feeds (iTunes, Podcasting
//! 2.0, content and Media RSS namespaces), on quick-xml.
//!
//! Namespace prefixes are resolved from the feed's own `xmlns:` declarations,
//! so a feed that binds the iTunes namespace to `it:` reads the same as one
//! that uses `itunes:`.

use std::collections::HashMap;

use anyhow::{Result, bail};
use quick_xml::events::{BytesStart, Event};
use serde::{Deserialize, Serialize};

use super::dates;

/// A show as its feed describes it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Feed {
    pub title: String,
    pub link: String,
    pub description: String,
    pub language: Option<String>,
    pub author: Option<String>,
    pub image: Option<String>,
    pub explicit: Option<bool>,
    /// `podcast:guid`.
    pub podcast_guid: Option<String>,
    /// `podcast:locked` (true = "yes").
    pub locked: Option<bool>,
    /// `atom:link rel="self"`.
    pub self_url: Option<String>,
    pub episodes: Vec<Episode>,
}

/// The media file of an episode.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enclosure {
    pub url: String,
    pub length: Option<u64>,
    pub mime: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    pub title: String,
    pub guid: Option<String>,
    pub link: Option<String>,
    pub description: Option<String>,
    /// `pubDate` as written.
    pub pub_date: Option<String>,
    /// `pubDate` as a Unix time, when it parses.
    pub published: Option<i64>,
    pub duration_secs: Option<u64>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub episode_type: Option<String>,
    pub image: Option<String>,
    pub enclosure: Option<Enclosure>,
}

impl Episode {
    /// A stable key: the guid, else the enclosure URL, else the title.
    pub fn key(&self) -> &str {
        self.guid
            .as_deref()
            .or(self.enclosure.as_ref().map(|e| e.url.as_str()))
            .unwrap_or(&self.title)
    }
}

const NAMESPACES: &[(&str, &str)] = &[
    ("http://www.itunes.com/dtds/podcast-1.0.dtd", "itunes"),
    ("https://www.itunes.com/dtds/podcast-1.0.dtd", "itunes"),
    ("https://podcastindex.org/namespace/1.0", "podcast"),
    ("http://podcastindex.org/namespace/1.0", "podcast"),
    ("https://github.com/podcastindex-org/podcast-namespace/blob/main/docs/1.0.md", "podcast"),
    ("http://purl.org/rss/1.0/modules/content/", "content"),
    ("http://www.w3.org/2005/atom", "atom"),
    ("http://search.yahoo.com/mrss/", "media"),
];

/// Most bytes a feed may be.
pub const MAX_FEED_BYTES: usize = 32 * 1024 * 1024;

struct Ns(HashMap<String, String>);

impl Ns {
    fn learn(&mut self, e: &BytesStart<'_>) {
        for a in e.attributes().flatten() {
            let key = String::from_utf8_lossy(a.key.as_ref()).to_string();
            if let Some(prefix) = key.strip_prefix("xmlns:") {
                let uri = String::from_utf8_lossy(&a.value).trim().to_ascii_lowercase();
                if let Some((_, canon)) = NAMESPACES.iter().find(|(u, _)| *u == uri) {
                    self.0.insert(prefix.to_string(), canon.to_string());
                }
            }
        }
    }

    fn canon(&self, raw: &[u8]) -> String {
        let name = String::from_utf8_lossy(raw);
        match name.split_once(':') {
            Some((p, local)) => match self.0.get(p) {
                Some(c) => format!("{c}:{local}"),
                None => name.to_string(),
            },
            None => name.to_string(),
        }
    }
}

fn attrs(e: &BytesStart<'_>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for a in e.attributes().with_checks(false).flatten() {
        let key = String::from_utf8_lossy(a.key.as_ref()).to_string();
        let value = match a.unescape_value() {
            Ok(v) => v.to_string(),
            Err(_) => String::from_utf8_lossy(&a.value).to_string(),
        };
        out.insert(key, value);
    }
    out
}

fn yes_no(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "yes" | "true" | "explicit" => Some(true),
        "no" | "false" | "clean" => Some(false),
        _ => None,
    }
}

fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Parse a feed document.
pub fn parse(xml: &str) -> Result<Feed> {
    if xml.len() > MAX_FEED_BYTES {
        bail!("feed is over {MAX_FEED_BYTES} bytes");
    }
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().check_end_names = false;
    let mut ns = Ns(HashMap::new());
    let mut stack: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut feed = Feed::default();
    let mut item: Option<Episode> = None;
    let mut saw_rss = false;
    let mut saw_channel = false;

    loop {
        let ev = match reader.read_event() {
            Ok(ev) => ev,
            Err(e) => bail!("feed is not well-formed XML at byte {}: {e}", reader.buffer_position()),
        };
        match ev {
            Event::Start(e) => {
                ns.learn(&e);
                let name = ns.canon(e.name().as_ref());
                on_open(&name, &e, &stack, &mut feed, &mut item);
                match name.as_str() {
                    "rss" | "rdf:RDF" => saw_rss = true,
                    "channel" => saw_channel = true,
                    "item" if stack.last().map(String::as_str) == Some("channel") => {
                        item = Some(Episode::default())
                    }
                    _ => {}
                }
                stack.push(name);
                text.clear();
            }
            Event::Empty(e) => {
                ns.learn(&e);
                let name = ns.canon(e.name().as_ref());
                on_open(&name, &e, &stack, &mut feed, &mut item);
            }
            Event::Text(t) => match t.unescape() {
                Ok(s) => text.push_str(&s),
                Err(_) => text.push_str(&String::from_utf8_lossy(&t)),
            },
            Event::CData(c) => text.push_str(&String::from_utf8_lossy(&c.into_inner())),
            Event::End(_) => {
                let Some(name) = stack.pop() else { continue };
                let parent = stack.last().map(String::as_str).unwrap_or("");
                let grand = stack.len().checked_sub(2).map(|i| stack[i].as_str()).unwrap_or("");
                let value = std::mem::take(&mut text);
                if name == "item" && parent == "channel" {
                    if let Some(mut ep) = item.take() {
                        ep.published = ep.pub_date.as_deref().and_then(dates::parse_rfc2822);
                        feed.episodes.push(ep);
                    }
                    continue;
                }
                match parent {
                    "channel" => on_channel_text(&name, &value, &mut feed),
                    "item" if grand == "channel" => {
                        if let Some(ep) = item.as_mut() {
                            on_item_text(&name, &value, ep);
                        }
                    }
                    "image" if grand == "channel" && name == "url" && feed.image.is_none() => {
                        feed.image = nonempty(&value)
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_rss || !saw_channel {
        bail!("not an RSS feed (no <rss><channel>)");
    }
    Ok(feed)
}

fn on_open(name: &str, e: &BytesStart<'_>, stack: &[String], feed: &mut Feed, item: &mut Option<Episode>) {
    let parent = stack.last().map(String::as_str).unwrap_or("");
    match (parent, name) {
        ("channel", "itunes:image") => {
            if let Some(h) = attrs(e).get("href").and_then(|h| nonempty(h)) {
                feed.image = Some(h);
            }
        }
        ("channel", "atom:link") => {
            let a = attrs(e);
            if a.get("rel").map(String::as_str) == Some("self") {
                feed.self_url = a.get("href").and_then(|h| nonempty(h));
            }
        }
        ("item", "enclosure") => {
            if let Some(ep) = item.as_mut() {
                let a = attrs(e);
                if let Some(url) = a.get("url").and_then(|u| nonempty(u)) {
                    ep.enclosure = Some(Enclosure {
                        url,
                        length: a.get("length").and_then(|l| l.trim().parse().ok()),
                        mime: a.get("type").and_then(|t| nonempty(t)),
                    });
                }
            }
        }
        ("item", "media:content") => {
            if let Some(ep) = item.as_mut()
                && ep.enclosure.is_none()
            {
                let a = attrs(e);
                if let Some(url) = a.get("url").and_then(|u| nonempty(u)) {
                    ep.enclosure = Some(Enclosure {
                        url,
                        length: a.get("fileSize").and_then(|l| l.trim().parse().ok()),
                        mime: a.get("type").and_then(|t| nonempty(t)),
                    });
                }
            }
        }
        ("item", "itunes:image") => {
            if let Some(ep) = item.as_mut() {
                ep.image = attrs(e).get("href").and_then(|h| nonempty(h));
            }
        }
        _ => {}
    }
}

fn on_channel_text(name: &str, v: &str, feed: &mut Feed) {
    match name {
        "title" => feed.title = v.trim().to_string(),
        "link" => {
            if feed.link.is_empty() {
                feed.link = v.trim().to_string()
            }
        }
        "description" => feed.description = v.trim().to_string(),
        "itunes:summary" if feed.description.is_empty() => feed.description = v.trim().to_string(),
        "language" => feed.language = nonempty(v),
        "itunes:author" => feed.author = nonempty(v),
        "managingEditor" if feed.author.is_none() => feed.author = nonempty(v),
        "itunes:explicit" => feed.explicit = yes_no(v),
        "podcast:guid" => feed.podcast_guid = nonempty(v),
        "podcast:locked" => feed.locked = yes_no(v),
        _ => {}
    }
}

fn on_item_text(name: &str, v: &str, ep: &mut Episode) {
    match name {
        "title" => ep.title = v.trim().to_string(),
        "itunes:title" if ep.title.is_empty() => ep.title = v.trim().to_string(),
        "link" => ep.link = nonempty(v),
        "guid" => ep.guid = nonempty(v),
        "description" => ep.description = nonempty(v),
        "content:encoded" | "itunes:summary" if ep.description.is_none() => ep.description = nonempty(v),
        "pubDate" => ep.pub_date = nonempty(v),
        "itunes:duration" => ep.duration_secs = dates::parse_duration(v),
        "itunes:season" => ep.season = v.trim().parse().ok(),
        "itunes:episode" => ep.episode = v.trim().parse().ok(),
        "itunes:episodeType" => ep.episode_type = nonempty(v),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like a big-host feed (Libsyn/Megaphone style): CDATA, tracking
    /// redirects in enclosures, itunes everything, an <image> block.
    const HOSTED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss xmlns:atom="http://www.w3.org/2005/Atom" xmlns:content="http://purl.org/rss/1.0/modules/content/" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd" xmlns:podcast="https://podcastindex.org/namespace/1.0" version="2.0">
  <channel>
    <atom:link href="https://feeds.example.com/show" rel="self" type="application/rss+xml"/>
    <title><![CDATA[The Example Hour]]></title>
    <link>https://example.com</link>
    <language>en-us</language>
    <copyright>&#169; 2026 Example &amp; Co</copyright>
    <description><![CDATA[<p>Weekly talk about <b>things</b>.</p>]]></description>
    <image>
      <url>https://img.example.com/fallback.jpg</url>
      <title>The Example Hour</title>
      <link>https://example.com</link>
    </image>
    <itunes:image href="https://img.example.com/cover.jpg"/>
    <itunes:author>Example &amp; Co</itunes:author>
    <itunes:explicit>false</itunes:explicit>
    <itunes:category text="Technology"><itunes:category text="Tech News"/></itunes:category>
    <podcast:guid>917393e3-1b1e-5cef-ace4-edaa54e1f810</podcast:guid>
    <podcast:locked owner="host@example.com">yes</podcast:locked>
    <item>
      <title>Ep. 2: Two</title>
      <description><![CDATA[Second <i>one</i>]]></description>
      <pubDate>Tue, 09 Jun 2026 10:00:00 -0400</pubDate>
      <guid isPermaLink="false"><![CDATA[abc-2]]></guid>
      <enclosure url="https://dts.podtrac.com/redirect.mp3/cdn.example.com/ep2.mp3?x=1&amp;y=2" length="48123904" type="audio/mpeg"/>
      <itunes:duration>50:08</itunes:duration>
      <itunes:episode>2</itunes:episode>
      <itunes:season>1</itunes:season>
      <itunes:episodeType>full</itunes:episodeType>
      <itunes:image href="https://img.example.com/ep2.jpg"/>
    </item>
    <item>
      <title>Ep. 1: One</title>
      <content:encoded><![CDATA[<p>First</p>]]></content:encoded>
      <pubDate>Tue, 02 Jun 2026 14:00:00 GMT</pubDate>
      <guid>https://example.com/ep1</guid>
      <enclosure url="https://cdn.example.com/ep1.m4a" length="" type="audio/x-m4a"/>
      <itunes:duration>3612</itunes:duration>
    </item>
  </channel>
</rss>"#;

    /// A hand-rolled feed: odd prefix for the iTunes namespace, no guid, no
    /// pubDate, media:content instead of enclosure, stray HTML entities.
    const HANDMADE: &str = r#"<rss version="2.0" xmlns:it="http://www.itunes.com/DTDs/Podcast-1.0.dtd" xmlns:media="http://search.yahoo.com/mrss/">
<channel>
<title>Garage &amp; Radio</title>
<link>http://garage.example.org/</link>
<description>we talk&nbsp;stuff</description>
<it:author>Two People</it:author>
<it:explicit>yes</it:explicit>
<item>
<title>first try</title>
<media:content url="http://garage.example.org/a.ogg" type="audio/ogg" fileSize="1234"/>
<it:duration>1:02:03</it:duration>
</item>
<item><title>no audio yet</title></item>
</channel>
</rss>"#;

    #[test]
    fn reads_a_hosted_feed() {
        let f = parse(HOSTED).unwrap();
        assert_eq!(f.title, "The Example Hour");
        assert_eq!(f.link, "https://example.com");
        assert_eq!(f.description, "<p>Weekly talk about <b>things</b>.</p>");
        assert_eq!(f.image.as_deref(), Some("https://img.example.com/cover.jpg"));
        assert_eq!(f.author.as_deref(), Some("Example & Co"));
        assert_eq!(f.explicit, Some(false));
        assert_eq!(f.locked, Some(true));
        assert_eq!(f.podcast_guid.as_deref(), Some("917393e3-1b1e-5cef-ace4-edaa54e1f810"));
        assert_eq!(f.self_url.as_deref(), Some("https://feeds.example.com/show"));
        assert_eq!(f.episodes.len(), 2);
        let e = &f.episodes[0];
        assert_eq!(e.title, "Ep. 2: Two");
        assert_eq!(e.guid.as_deref(), Some("abc-2"));
        assert_eq!(e.description.as_deref(), Some("Second <i>one</i>"));
        let enc = e.enclosure.as_ref().unwrap();
        assert_eq!(enc.url, "https://dts.podtrac.com/redirect.mp3/cdn.example.com/ep2.mp3?x=1&y=2");
        assert_eq!(enc.length, Some(48_123_904));
        assert_eq!(enc.mime.as_deref(), Some("audio/mpeg"));
        assert_eq!(e.duration_secs, Some(50 * 60 + 8));
        assert_eq!((e.season, e.episode), (Some(1), Some(2)));
        assert_eq!(e.image.as_deref(), Some("https://img.example.com/ep2.jpg"));
        assert_eq!(e.published, dates::parse_date_input("2026-06-09T14:00:00Z"));
        let e1 = &f.episodes[1];
        assert_eq!(e1.description.as_deref(), Some("<p>First</p>"));
        assert_eq!(e1.enclosure.as_ref().unwrap().length, None);
        assert_eq!(e1.duration_secs, Some(3612));
        assert_eq!(e1.key(), "https://example.com/ep1");
    }

    #[test]
    fn reads_a_handmade_feed() {
        let f = parse(HANDMADE).unwrap();
        assert_eq!(f.title, "Garage & Radio");
        assert_eq!(f.author.as_deref(), Some("Two People"));
        assert_eq!(f.explicit, Some(true));
        assert!(f.description.contains("stuff"));
        assert_eq!(f.episodes.len(), 2);
        let e = &f.episodes[0];
        let enc = e.enclosure.as_ref().unwrap();
        assert_eq!(enc.url, "http://garage.example.org/a.ogg");
        assert_eq!(enc.length, Some(1234));
        assert_eq!(e.duration_secs, Some(3723));
        assert_eq!(e.key(), "http://garage.example.org/a.ogg");
        assert!(f.episodes[1].enclosure.is_none());
        assert_eq!(f.episodes[1].key(), "no audio yet");
    }

    #[test]
    fn refuses_non_feeds() {
        assert!(parse("<html><body>hi</body></html>").is_err());
        assert!(parse("not xml at all <<<").is_err());
    }
}
