//! HLS playlists: writing the live media / master playlists a gateway serves
//! (RFC 8216), and reading the local playlist ffmpeg's HLS muxer writes so
//! the broadcaster knows when a segment is complete.

use std::fmt::Write as _;

use super::record::{LiveRecord, Rendition};

/// Segment file extension (MPEG-TS segments in v1).
pub const SEGMENT_EXT: &str = "ts";
pub const SEGMENT_CONTENT_TYPE: &str = "video/mp2t";
pub const PLAYLIST_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";

/// `EXT-X-TARGETDURATION`: the longest segment, rounded up to whole seconds
/// (RFC 8216 4.3.3.1). At least 1.
pub fn target_duration(r: &Rendition) -> u64 {
    r.segments
        .iter()
        .map(|s| (s.dur_ms as u64).div_ceil(1000))
        .max()
        .unwrap_or(1)
        .max(1)
}

/// A live media playlist for one rendition. Segment URIs are relative
/// (`<hash>.ts`), so the playlist works under any gateway path.
pub fn media_playlist(r: &Rendition, ended: bool) -> String {
    let mut out = String::with_capacity(128 + r.segments.len() * 96);
    out.push_str("#EXTM3U\n#EXT-X-VERSION:3\n");
    let _ = writeln!(out, "#EXT-X-TARGETDURATION:{}", target_duration(r));
    let _ = writeln!(out, "#EXT-X-MEDIA-SEQUENCE:{}", r.media_seq);
    if r.disc_seq > 0 {
        let _ = writeln!(out, "#EXT-X-DISCONTINUITY-SEQUENCE:{}", r.disc_seq);
    }
    out.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
    for s in &r.segments {
        if s.disc {
            out.push_str("#EXT-X-DISCONTINUITY\n");
        }
        let _ = writeln!(out, "#EXTINF:{}.{:03},", s.dur_ms / 1000, s.dur_ms % 1000);
        let _ = writeln!(out, "{}.{SEGMENT_EXT}", s.hash);
    }
    if ended {
        out.push_str("#EXT-X-ENDLIST\n");
    }
    out
}

/// Peak and average bits per second of a rendition's window (for `BANDWIDTH`
/// / `AVERAGE-BANDWIDTH`). A rendition with no segments yet reports 1.
pub fn bandwidth(r: &Rendition) -> (u64, u64) {
    let mut peak = 0u64;
    let (mut bytes, mut ms) = (0u64, 0u64);
    for s in &r.segments {
        peak = peak.max(s.size * 8 * 1000 / s.dur_ms.max(1) as u64);
        bytes += s.size;
        ms += s.dur_ms as u64;
    }
    let avg = if ms == 0 { 0 } else { bytes * 8 * 1000 / ms };
    (peak.max(1), avg.max(1))
}

/// A master playlist listing each rendition's media playlist at
/// `<rendition>/index.m3u8`, best first.
pub fn master_playlist(rec: &LiveRecord) -> String {
    let mut rs: Vec<&Rendition> = rec.body.renditions.iter().collect();
    rs.sort_by_key(|r| std::cmp::Reverse(bandwidth(r).0));
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    for r in rs {
        let (peak, avg) = bandwidth(r);
        let _ = write!(out, "#EXT-X-STREAM-INF:BANDWIDTH={peak},AVERAGE-BANDWIDTH={avg}");
        if let Some(res) = &r.resolution {
            let _ = write!(out, ",RESOLUTION={res}");
        }
        let _ = writeln!(out, "\n{}/index.m3u8", r.name);
    }
    out
}

/// What `/live/<id>/index.m3u8` serves: the media playlist itself for a
/// single-rendition stream, a master playlist otherwise.
pub fn index_playlist(rec: &LiveRecord) -> String {
    match rec.body.renditions.as_slice() {
        [only] => media_playlist(only, rec.body.ended),
        _ => master_playlist(rec),
    }
}

/// One entry of a playlist ffmpeg wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalSegment {
    pub uri: String,
    pub dur_ms: u32,
    pub disc: bool,
    /// Media sequence number ffmpeg gave it.
    pub seq: u64,
}

/// A parsed local media playlist.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalPlaylist {
    pub segments: Vec<LocalSegment>,
    pub ended: bool,
}

/// Parse a media playlist (what ffmpeg's HLS muxer writes). Lenient: unknown
/// tags are skipped. Only complete entries (`#EXTINF` followed by a URI) count.
pub fn parse_media_playlist(text: &str) -> LocalPlaylist {
    let mut out = LocalPlaylist::default();
    let mut seq = 0u64;
    let mut pending_dur: Option<u32> = None;
    let mut pending_disc = false;
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXTINF:") {
            let num = v.split(',').next().unwrap_or("").trim();
            pending_dur = parse_seconds_ms(num);
        } else if line == "#EXT-X-DISCONTINUITY" {
            pending_disc = true;
        } else if line == "#EXT-X-ENDLIST" {
            out.ended = true;
        } else if line.starts_with('#') {
            continue;
        } else if let Some(dur_ms) = pending_dur.take() {
            out.segments.push(LocalSegment {
                uri: line.to_string(),
                dur_ms: dur_ms.max(1),
                disc: std::mem::take(&mut pending_disc),
                seq,
            });
            seq += 1;
        }
    }
    out
}

/// "4.004000" -> 4004. Rounded to the nearest millisecond.
fn parse_seconds_ms(s: &str) -> Option<u32> {
    let f: f64 = s.parse().ok()?;
    if !f.is_finite() || f < 0.0 || f > 3600.0 {
        return None;
    }
    Some((f * 1000.0).round() as u32)
}

#[cfg(test)]
mod tests {
    use super::super::record::{Window, describe_segment};
    use super::*;

    fn rendition(durs: &[u32], discs: &[usize], window: usize) -> Rendition {
        let mut w = Window::new("src", Some("640x360".into()), window);
        for (i, d) in durs.iter().enumerate() {
            let (s, _) = describe_segment(format!("seg{i}").as_bytes(), *d, discs.contains(&i));
            w.push(s);
        }
        w.rendition
    }

    #[test]
    fn media_playlist_has_valid_live_tags() {
        let r = rendition(&[4000, 4004, 3500, 4200], &[], 6);
        let p = media_playlist(&r, false);
        let lines: Vec<&str> = p.lines().collect();
        assert_eq!(lines[0], "#EXTM3U");
        assert!(p.contains("#EXT-X-VERSION:3\n"));
        assert!(p.contains("#EXT-X-TARGETDURATION:5\n"), "4.2 s rounds up to 5: {p}");
        assert!(p.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
        assert!(!p.contains("#EXT-X-ENDLIST"), "live playlists stay open");
        assert!(!p.contains("DISCONTINUITY"));
        assert!(p.contains("#EXTINF:4.004,\n"));
        assert!(p.contains("#EXTINF:3.500,\n"));
        let uris: Vec<&&str> = lines.iter().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(uris.len(), 4);
        assert_eq!(*uris[0], format!("{}.ts", r.segments[0].hash));
        // every EXTINF is <= target duration (RFC 8216 4.3.3.1)
        for s in &r.segments {
            assert!((s.dur_ms as u64).div_ceil(1000) <= target_duration(&r));
        }
        assert!(media_playlist(&r, true).ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn media_sequence_follows_the_window() {
        let r = rendition(&[2000; 10], &[], 4);
        let p = media_playlist(&r, false);
        assert!(p.contains("#EXT-X-MEDIA-SEQUENCE:6\n"), "{p}");
        assert_eq!(p.matches("#EXTINF").count(), 4);
    }

    #[test]
    fn discontinuities_tagged_and_counted() {
        // segment 2 and 5 follow encoder restarts; window of 4 over 7 segments
        let r = rendition(&[2000; 7], &[2, 5], 4);
        // window is segments 3..=6: only 5 is tagged; 2 left -> sequence 1
        let p = media_playlist(&r, false);
        assert!(p.contains("#EXT-X-DISCONTINUITY-SEQUENCE:1\n"), "{p}");
        assert_eq!(p.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
        // the tag sits right before the restarted segment's EXTINF
        let tag = p.find("#EXT-X-DISCONTINUITY\n").unwrap();
        let next_uri = p[tag..].lines().find(|l| !l.starts_with('#')).unwrap();
        assert_eq!(next_uri, format!("{}.ts", r.segments[2].hash));
    }

    #[test]
    fn master_playlist_lists_renditions() {
        let k = c0mpute_net::libp2p::identity::Keypair::generate_ed25519();
        let mut hi = rendition(&[2000, 2000], &[], 6);
        hi.name = "src".into();
        hi.resolution = Some("1280x720".into());
        let mut lo = Window::new("480p", Some("854x480".into()), 6);
        lo.push(describe_segment(b"x", 2000, false).0);
        let rec = LiveRecord::sign(
            &k,
            super::super::record::LiveBody {
                v: 1,
                stream_id: String::new(),
                name: "two".into(),
                owner: String::new(),
                seq: 1,
                ts_ms: 1,
                ended: false,
                renditions: vec![lo.rendition, hi],
            },
        )
        .unwrap();
        let m = index_playlist(&rec);
        assert!(m.starts_with("#EXTM3U\n"));
        assert_eq!(m.matches("#EXT-X-STREAM-INF:BANDWIDTH=").count(), 2);
        assert!(m.contains("RESOLUTION=1280x720\nsrc/index.m3u8\n"));
        assert!(m.contains("RESOLUTION=854x480\n480p/index.m3u8\n"));
        assert!(m.find("src/index.m3u8").unwrap() < m.find("480p/index.m3u8").unwrap(), "best first");
    }

    #[test]
    fn parses_ffmpeg_playlists() {
        let text = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:12\n\
                    #EXTINF:4.004000,\nseg12.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:3.970000,\nseg13.ts\n\
                    #EXTINF:2.000000,\n";
        let p = parse_media_playlist(text);
        assert_eq!(p.segments.len(), 2, "a trailing EXTINF without URI is incomplete");
        assert_eq!(p.segments[0], LocalSegment { uri: "seg12.ts".into(), dur_ms: 4004, disc: false, seq: 12 });
        assert_eq!(p.segments[1].dur_ms, 3970);
        assert!(p.segments[1].disc);
        assert_eq!(p.segments[1].seq, 13);
        assert!(!p.ended);
        assert!(parse_media_playlist("#EXTM3U\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n").ended);
    }

    #[test]
    fn round_trips_our_own_playlist() {
        let r = rendition(&[2000, 2500, 3000, 1999], &[1], 6);
        let back = parse_media_playlist(&media_playlist(&r, true));
        assert!(back.ended);
        assert_eq!(back.segments.len(), 4);
        for (a, b) in back.segments.iter().zip(&r.segments) {
            assert_eq!(a.dur_ms, b.dur_ms);
            assert_eq!(a.disc, b.disc);
            assert_eq!(a.uri, format!("{}.ts", b.hash));
        }
    }
}
