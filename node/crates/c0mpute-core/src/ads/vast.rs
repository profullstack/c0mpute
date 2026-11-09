//! VAST 4.2 and VMAP 1.0 documents for video players (connected TV, web).
//!
//! An InLine ad carries every element VAST 4 requires: `AdSystem`, `Error`,
//! `Impression`, `AdServingId`, `AdTitle`, and a `Creative` with
//! `UniversalAdId` and a `Linear` (`Duration`, `MediaFiles`). Media and every
//! tracking URL point back at the gateway that answered, which serves the
//! creative from the hosting swarm and turns each beacon into a signed
//! receipt. No fill is the empty `<VAST version="4.2"/>`, as the spec says.

use std::fmt::Write;

use crate::podcast::show::esc;

pub const VAST_VERSION: &str = "4.2";

/// Everything one linear video ad needs.
#[derive(Clone, Debug)]
pub struct VastAd {
    pub ad_id: String,
    pub serving_id: String,
    pub title: String,
    pub advertiser: String,
    pub creative_id: String,
    pub media_url: String,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    pub bytes: u64,
    pub impression_url: String,
    /// (VAST event name, URL), e.g. ("firstQuartile", ...).
    pub tracking: Vec<(String, String)>,
    pub click_through: String,
    pub click_tracking: String,
    pub error_url: String,
}

/// `HH:MM:SS.mmm`.
pub fn fmt_duration(ms: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        ms % 1000
    )
}

fn cdata(s: &str) -> String {
    // A URL never contains "]]>", but split it if one does.
    format!("<![CDATA[{}]]>", s.replace("]]>", "]]]]><![CDATA[>"))
}

/// No fill.
pub fn empty() -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<VAST version=\"{VAST_VERSION}\" xmlns=\"http://www.iab.com/VAST\"/>\n")
}

pub fn inline(ad: &VastAd) -> String {
    let mut x = String::new();
    x.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(x, "<VAST version=\"{VAST_VERSION}\" xmlns=\"http://www.iab.com/VAST\">");
    let _ = writeln!(x, "  <Ad id=\"{}\">", esc(&ad.ad_id));
    x.push_str("    <InLine>\n");
    let _ = writeln!(x, "      <AdSystem version=\"{}\">c0mpute</AdSystem>", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(x, "      <Error>{}</Error>", cdata(&ad.error_url));
    let _ = writeln!(x, "      <Impression id=\"c0mpute\">{}</Impression>", cdata(&ad.impression_url));
    let _ = writeln!(x, "      <AdServingId>{}</AdServingId>", esc(&ad.serving_id));
    let _ = writeln!(x, "      <AdTitle>{}</AdTitle>", esc(&ad.title));
    let _ = writeln!(x, "      <Advertiser>{}</Advertiser>", esc(&ad.advertiser));
    x.push_str("      <Creatives>\n");
    let _ = writeln!(
        x,
        "        <Creative id=\"{}\" adId=\"{}\" sequence=\"1\">",
        esc(&ad.creative_id),
        esc(&ad.ad_id)
    );
    let _ = writeln!(
        x,
        "          <UniversalAdId idRegistry=\"c0mpute\">{}</UniversalAdId>",
        esc(&ad.creative_id)
    );
    x.push_str("          <Linear>\n");
    let _ = writeln!(x, "            <Duration>{}</Duration>", fmt_duration(ad.duration_ms));
    x.push_str("            <TrackingEvents>\n");
    for (ev, url) in &ad.tracking {
        let _ = writeln!(x, "              <Tracking event=\"{}\">{}</Tracking>", esc(ev), cdata(url));
    }
    x.push_str("            </TrackingEvents>\n");
    x.push_str("            <VideoClicks>\n");
    let _ = writeln!(x, "              <ClickThrough id=\"c0mpute\">{}</ClickThrough>", cdata(&ad.click_through));
    let _ = writeln!(x, "              <ClickTracking id=\"c0mpute\">{}</ClickTracking>", cdata(&ad.click_tracking));
    x.push_str("            </VideoClicks>\n");
    x.push_str("            <MediaFiles>\n");
    let kbps = if ad.duration_ms > 0 {
        (ad.bytes * 8 / ad.duration_ms).max(1)
    } else {
        0
    };
    let _ = writeln!(
        x,
        "              <MediaFile id=\"{}\" delivery=\"progressive\" type=\"{}\" width=\"{}\" height=\"{}\" bitrate=\"{}\" scalable=\"true\" maintainAspectRatio=\"true\">{}</MediaFile>",
        esc(&ad.creative_id),
        esc(&ad.mime),
        ad.width,
        ad.height,
        kbps,
        cdata(&ad.media_url)
    );
    x.push_str("            </MediaFiles>\n");
    x.push_str("          </Linear>\n");
    x.push_str("        </Creative>\n");
    x.push_str("      </Creatives>\n");
    x.push_str("    </InLine>\n");
    x.push_str("  </Ad>\n");
    x.push_str("</VAST>\n");
    x
}

/// One break in a VMAP: where (`start`, `end`, or `HH:MM:SS.mmm`) and the
/// VAST tag that fills it.
#[derive(Clone, Debug)]
pub struct Break {
    pub id: String,
    pub time_offset: String,
    pub vast_url: String,
}

/// A break offset from user input: `start`/`pre`, `end`/`post`, seconds, or
/// `HH:MM:SS(.mmm)`.
pub fn parse_offset(s: &str) -> Option<String> {
    let s = s.trim();
    match s {
        "start" | "pre" | "preroll" => return Some("start".into()),
        "end" | "post" | "postroll" => return Some("end".into()),
        _ => {}
    }
    if let Ok(secs) = s.parse::<u64>() {
        return Some(fmt_duration(secs * 1000));
    }
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() == 3 {
        let h: u64 = parts[0].parse().ok()?;
        let m: u64 = parts[1].parse().ok()?;
        let (sec, ms) = match parts[2].split_once('.') {
            Some((a, b)) => (a.parse::<u64>().ok()?, format!("{b:0<3}")[..3].parse::<u64>().ok()?),
            None => (parts[2].parse().ok()?, 0),
        };
        if m >= 60 || sec >= 60 {
            return None;
        }
        return Some(fmt_duration(((h * 60 + m) * 60 + sec) * 1000 + ms));
    }
    None
}

pub fn vmap(breaks: &[Break]) -> String {
    let mut x = String::new();
    x.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    x.push_str("<vmap:VMAP xmlns:vmap=\"http://www.iab.net/videosuite/vmap\" version=\"1.0\">\n");
    for b in breaks {
        let kind = "linear";
        let _ = writeln!(
            x,
            "  <vmap:AdBreak timeOffset=\"{}\" breakType=\"{kind}\" breakId=\"{}\">",
            esc(&b.time_offset),
            esc(&b.id)
        );
        let _ = writeln!(
            x,
            "    <vmap:AdSource id=\"{}-ad\" allowMultipleAds=\"false\" followRedirects=\"true\">",
            esc(&b.id)
        );
        let _ = writeln!(x, "      <vmap:AdTagURI templateType=\"vast4\">{}</vmap:AdTagURI>", cdata(&b.vast_url));
        x.push_str("    </vmap:AdSource>\n");
        x.push_str("  </vmap:AdBreak>\n");
    }
    x.push_str("</vmap:VMAP>\n");
    x
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use quick_xml::Reader;
    use quick_xml::events::Event;

    /// A tiny DOM: (path of local element names, attributes, text).
    pub type Node = (Vec<String>, Vec<(String, String)>, String);

    /// Parse to a flat list of elements; panics if the XML is not well formed
    /// (unbalanced tags, bad escapes).
    pub fn parse(xml: &str) -> Vec<Node> {
        let mut r = Reader::from_str(xml);
        let mut stack: Vec<String> = Vec::new();
        let mut out: Vec<Node> = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        loop {
            match r.read_event().expect("well-formed XML") {
                Event::Start(e) => {
                    let name = String::from_utf8(e.name().as_ref().to_vec()).unwrap();
                    stack.push(name);
                    let attrs = e
                        .attributes()
                        .map(|a| {
                            let a = a.expect("attribute");
                            (
                                String::from_utf8(a.key.as_ref().to_vec()).unwrap(),
                                a.unescape_value().expect("attr escape").to_string(),
                            )
                        })
                        .collect();
                    out.push((stack.clone(), attrs, String::new()));
                    open.push(out.len() - 1);
                }
                Event::Empty(e) => {
                    let name = String::from_utf8(e.name().as_ref().to_vec()).unwrap();
                    let mut path = stack.clone();
                    path.push(name);
                    let attrs = e
                        .attributes()
                        .map(|a| {
                            let a = a.expect("attribute");
                            (
                                String::from_utf8(a.key.as_ref().to_vec()).unwrap(),
                                a.unescape_value().expect("attr escape").to_string(),
                            )
                        })
                        .collect();
                    out.push((path, attrs, String::new()));
                }
                Event::End(e) => {
                    let name = String::from_utf8(e.name().as_ref().to_vec()).unwrap();
                    assert_eq!(stack.pop().as_deref(), Some(name.as_str()), "balanced tags");
                    open.pop();
                }
                Event::Text(t) => {
                    if let Some(i) = open.last() {
                        out[*i].2.push_str(t.unescape().expect("text escape").trim());
                    }
                }
                Event::CData(t) => {
                    if let Some(i) = open.last() {
                        out[*i].2.push_str(std::str::from_utf8(&t.into_inner()).unwrap().trim());
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        assert!(stack.is_empty(), "unclosed: {stack:?}");
        out
    }

    pub fn find<'a>(nodes: &'a [Node], path: &str) -> Vec<&'a Node> {
        let want: Vec<&str> = path.split('/').collect();
        nodes
            .iter()
            .filter(|(p, _, _)| p.iter().map(String::as_str).collect::<Vec<_>>() == want)
            .collect()
    }

    pub fn attr<'a>(n: &'a Node, k: &str) -> Option<&'a str> {
        n.1.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }

    /// The VAST 4 InLine structure: every required element present once,
    /// with sane values.
    pub fn assert_valid_vast4_inline(xml: &str) {
        let n = parse(xml);
        let root = find(&n, "VAST");
        assert_eq!(root.len(), 1);
        assert!(attr(root[0], "version").unwrap().starts_with('4'));
        let base = "VAST/Ad/InLine";
        assert_eq!(find(&n, "VAST/Ad").len(), 1);
        assert!(attr(find(&n, "VAST/Ad")[0], "id").is_some_and(|v| !v.is_empty()));
        for req in ["AdSystem", "AdServingId", "AdTitle", "Impression", "Creatives"] {
            let found = find(&n, &format!("{base}/{req}"));
            assert!(!found.is_empty(), "missing required {req}");
        }
        for (k, v) in [("AdSystem", 1), ("AdServingId", 1), ("AdTitle", 1)] {
            assert_eq!(find(&n, &format!("{base}/{k}")).len(), v, "{k}");
            assert!(!find(&n, &format!("{base}/{k}"))[0].2.is_empty(), "{k} has text");
        }
        assert!(find(&n, &format!("{base}/Impression"))[0].2.starts_with("http"));
        let cr = format!("{base}/Creatives/Creative");
        assert!(!find(&n, &cr).is_empty());
        assert!(!find(&n, &format!("{cr}/UniversalAdId"))[0].2.is_empty(), "UniversalAdId (4.1+)");
        assert!(attr(find(&n, &format!("{cr}/UniversalAdId"))[0], "idRegistry").is_some());
        let lin = format!("{cr}/Linear");
        let d = &find(&n, &format!("{lin}/Duration"))[0].2;
        assert!(parse_offset(d).is_some(), "Duration is HH:MM:SS.mmm: {d}");
        let mf = find(&n, &format!("{lin}/MediaFiles/MediaFile"));
        assert!(!mf.is_empty());
        for m in mf {
            for a in ["delivery", "type", "width", "height"] {
                assert!(attr(m, a).is_some(), "MediaFile@{a}");
            }
            assert!(matches!(attr(m, "delivery"), Some("progressive" | "streaming")));
            assert!(m.2.starts_with("http"), "MediaFile URL");
        }
        for t in find(&n, &format!("{lin}/TrackingEvents/Tracking")) {
            let ev = attr(t, "event").unwrap();
            assert!(
                ["start", "firstQuartile", "midpoint", "thirdQuartile", "complete", "creativeView", "pause", "resume", "skip", "mute", "unmute"]
                    .contains(&ev),
                "{ev}"
            );
            assert!(t.2.starts_with("http"));
        }
    }

    fn ad() -> VastAd {
        VastAd {
            ad_id: "c1".into(),
            serving_id: "s1".into(),
            title: "Spring <sale> & more".into(),
            advertiser: "Acme".into(),
            creative_id: "ab".repeat(32),
            media_url: "https://gw.example/ads/v1/creative/c1/abab".into(),
            mime: "video/mp4".into(),
            width: 1280,
            height: 720,
            duration_ms: 5000,
            bytes: 1_000_000,
            impression_url: "https://gw.example/ads/v1/t?ev=impression&tok=a.b".into(),
            tracking: ["start", "firstQuartile", "midpoint", "thirdQuartile", "complete"]
                .iter()
                .map(|e| (e.to_string(), format!("https://gw.example/ads/v1/t?ev={e}&tok=a.b")))
                .collect(),
            click_through: "https://example.org/?a=1&b=2".into(),
            click_tracking: "https://gw.example/ads/v1/t?ev=click&tok=a.b".into(),
            error_url: "https://gw.example/ads/v1/t?ev=error&code=[ERRORCODE]".into(),
        }
    }

    #[test]
    fn inline_is_valid_vast4() {
        let xml = inline(&ad());
        assert_valid_vast4_inline(&xml);
        let n = parse(&xml);
        assert_eq!(find(&n, "VAST/Ad/InLine/AdTitle")[0].2, "Spring <sale> & more");
        assert_eq!(find(&n, "VAST/Ad/InLine/Creatives/Creative/Linear/Duration")[0].2, "00:00:05.000");
        assert_eq!(
            find(&n, "VAST/Ad/InLine/Creatives/Creative/Linear/VideoClicks/ClickThrough")[0].2,
            "https://example.org/?a=1&b=2"
        );
        assert_eq!(find(&n, "VAST/Ad/InLine/Creatives/Creative/Linear/TrackingEvents/Tracking").len(), 5);
    }

    #[test]
    fn empty_is_no_fill() {
        let n = parse(&empty());
        assert_eq!(find(&n, "VAST").len(), 1);
        assert!(find(&n, "VAST/Ad").is_empty());
    }

    #[test]
    fn vmap_structure_and_offsets() {
        assert_eq!(parse_offset("start").unwrap(), "start");
        assert_eq!(parse_offset("post").unwrap(), "end");
        assert_eq!(parse_offset("600").unwrap(), "00:10:00.000");
        assert_eq!(parse_offset("01:02:03.5").unwrap(), "01:02:03.500");
        assert!(parse_offset("00:61:00").is_none());
        assert!(parse_offset("soon").is_none());
        let xml = vmap(&[
            Break {
                id: "preroll".into(),
                time_offset: "start".into(),
                vast_url: "https://gw/ads/v1/vast?placement=ctv_preroll&x=1".into(),
            },
            Break {
                id: "midroll-1".into(),
                time_offset: "00:10:00.000".into(),
                vast_url: "https://gw/ads/v1/vast?placement=ctv_midroll".into(),
            },
        ]);
        let n = parse(&xml);
        assert_eq!(attr(find(&n, "vmap:VMAP")[0], "version"), Some("1.0"));
        let breaks = find(&n, "vmap:VMAP/vmap:AdBreak");
        assert_eq!(breaks.len(), 2);
        for b in &breaks {
            for a in ["timeOffset", "breakType", "breakId"] {
                assert!(attr(b, a).is_some());
            }
        }
        let tags = find(&n, "vmap:VMAP/vmap:AdBreak/vmap:AdSource/vmap:AdTagURI");
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].2, "https://gw/ads/v1/vast?placement=ctv_preroll&x=1");
        assert_eq!(attr(tags[0], "templateType"), Some("vast4"));
    }
}
