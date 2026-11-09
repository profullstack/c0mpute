//! Folder pages for sites without an `index.html`.
//!
//! A site published from a folder of files (an album of MP3s, a set of
//! videos, a directory of PDFs) has nothing at `/` and every folder 404s,
//! although every file serves. Like a web server's autoindex, a gateway now
//! answers a folder request with no `index.html` with a generated page: the
//! subfolders, then the files with their sizes, and players for audio and
//! video. A folder asked for without its trailing slash redirects to it, so
//! relative links resolve. A site's own `index.html` always wins.

use std::collections::BTreeMap;

use super::Hosting;
use super::site::{FileEntry, Manifest, SiteRecord};

/// Most entries one folder page lists.
const MAX_ENTRIES: usize = 5_000;
/// Most manifest parts read for one folder of a large (v2) site.
const MAX_PARTS: usize = 64;

/// What a folder request turns into.
#[derive(Debug, PartialEq, Eq)]
pub enum Folder {
    /// `Location` relative to the request (`name/`): the folder with its slash.
    Redirect(String),
    /// A generated HTML page.
    Page(String),
}

/// One row of a folder page.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    Dir(String),
    File(String, u64, String),
}

fn decode(path: &str) -> String {
    let p = path.trim_start_matches('/');
    let p = p.split(['?', '#']).next().unwrap_or("");
    super::site::candidates(&format!("/{p}x"))
        .into_iter()
        .next()
        .map(|c| c.strip_suffix('x').unwrap_or(&c).to_string())
        .unwrap_or_default()
}

/// The rows directly under `dir` ("" or "a/b/") from paths in order.
fn rows_under<'a>(dir: &str, files: impl Iterator<Item = (&'a String, &'a FileEntry)>) -> Vec<Row> {
    let mut dirs: BTreeMap<String, ()> = BTreeMap::new();
    let mut out = Vec::new();
    for (path, f) in files {
        let Some(rest) = path.strip_prefix(dir) else { continue };
        if rest.is_empty() {
            continue;
        }
        match rest.split_once('/') {
            Some((sub, _)) => {
                dirs.insert(sub.to_string(), ());
            }
            None => out.push(Row::File(rest.to_string(), f.size, f.content_type.clone())),
        }
        if dirs.len() + out.len() >= MAX_ENTRIES {
            break;
        }
    }
    let mut rows: Vec<Row> = dirs.into_keys().map(Row::Dir).collect();
    rows.extend(out);
    rows
}

impl Hosting {
    /// The rows of folder `dir` of a site (empty when it has no files).
    async fn folder_rows(&self, rec: &SiteRecord, m: &Manifest, dir: &str) -> anyhow::Result<Vec<Row>> {
        match m {
            Manifest::Inline(sm) => Ok(rows_under(dir, sm.files.range(dir.to_string()..))),
            Manifest::Parted(root) => {
                // Parts are in path order: start at the one that would hold
                // `dir`, stop once a part starts past the folder.
                let start = root.parts.partition_point(|p| p.first.as_str() <= dir).saturating_sub(1);
                let mut files: BTreeMap<String, FileEntry> = BTreeMap::new();
                for pr in root.parts.iter().skip(start).take(MAX_PARTS) {
                    if pr.first.as_str() > dir && !pr.first.starts_with(dir) {
                        break;
                    }
                    let part = self.part(&rec.site_id, pr).await?;
                    files.extend(
                        part.files
                            .range(dir.to_string()..)
                            .take_while(|(p, _)| p.starts_with(dir))
                            .map(|(p, e)| (p.clone(), e.clone())),
                    );
                    if files.len() >= MAX_ENTRIES {
                        break;
                    }
                }
                Ok(rows_under(dir, files.iter()))
            }
        }
    }

    /// A folder page or redirect for `path`, when it names a folder with files
    /// (call only after the site's own files did not match).
    pub(super) async fn folder(&self, rec: &SiteRecord, m: &Manifest, path: &str) -> anyhow::Result<Option<Folder>> {
        let decoded = decode(path);
        if decoded.is_empty() || decoded.ends_with('/') {
            let rows = self.folder_rows(rec, m, &decoded).await?;
            if rows.is_empty() {
                return Ok(None);
            }
            return Ok(Some(Folder::Page(render(&rec.name, &decoded, &rows))));
        }
        let dir = format!("{decoded}/");
        if self.folder_rows(rec, m, &dir).await?.is_empty() {
            return Ok(None);
        }
        let last = decoded.rsplit('/').next().unwrap_or(&decoded);
        Ok(Some(Folder::Redirect(format!("{}/", encode_segment(last)))))
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Percent-encode one path segment for an href.
fn encode_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

fn render(site: &str, dir: &str, rows: &[Row]) -> String {
    let title = if dir.is_empty() { site.to_string() } else { format!("{site} / {}", dir.trim_end_matches('/')) };
    let mut crumbs = String::new();
    let depth = dir.matches('/').count();
    crumbs.push_str(&format!("<a href=\"{}\">{}</a>", if depth == 0 { "./".to_string() } else { "../".repeat(depth) }, esc(site)));
    let mut up = depth;
    for seg in dir.split('/').filter(|s| !s.is_empty()) {
        up -= 1;
        let href = if up == 0 { "./".to_string() } else { "../".repeat(up) };
        crumbs.push_str(&format!(" / <a href=\"{href}\">{}</a>", esc(seg)));
    }
    let mut list = String::new();
    let mut tracks = 0;
    if depth > 0 {
        list.push_str("<li class=\"dir\"><a href=\"../\">..</a></li>\n");
    }
    for row in rows {
        match row {
            Row::Dir(name) => list.push_str(&format!(
                "<li class=\"dir\"><a href=\"{}/\">{}/</a></li>\n",
                encode_segment(name),
                esc(name)
            )),
            Row::File(name, size, ct) => {
                let href = encode_segment(name);
                let media = if ct.starts_with("audio/") {
                    tracks += 1;
                    format!("<audio controls preload=\"none\" src=\"{href}\"></audio>")
                } else if ct.starts_with("video/") {
                    format!("<video controls preload=\"metadata\" src=\"{href}\"></video>")
                } else if ct.starts_with("image/") {
                    format!("<img loading=\"lazy\" alt=\"\" src=\"{href}\">")
                } else {
                    String::new()
                };
                list.push_str(&format!(
                    "<li class=\"file\"><a href=\"{href}\">{}</a> <span class=\"size\">{}</span>{media}</li>\n",
                    esc(name),
                    human(*size)
                ));
            }
        }
    }
    let play_all = if tracks > 1 {
        "<p><button type=\"button\" id=\"all\">Play all</button></p>\n<script>\
document.getElementById('all').onclick=()=>{const a=[...document.querySelectorAll('audio')];\
a.forEach((el,i)=>el.onended=()=>{if(a[i+1])a[i+1].play()});if(a[0])a[0].play()};\
document.addEventListener('play',e=>{document.querySelectorAll('audio,video').forEach(m=>{if(m!==e.target)m.pause()})},true);\
</script>\n"
    } else {
        ""
    };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{t}</title><meta property=\"og:title\" content=\"{t}\">\
<style>:root{{color-scheme:light dark}}body{{font:16px/1.5 system-ui,sans-serif;max-width:52rem;margin:2rem auto;padding:0 1rem}}\
ul{{list-style:none;padding:0}}li{{padding:.4rem 0;border-bottom:1px solid #8883}}.size{{opacity:.6;font-size:.85em;margin-left:.5rem}}\
audio,video{{display:block;width:100%;margin-top:.3rem}}img{{display:block;max-width:100%;max-height:16rem;margin-top:.3rem}}\
footer{{opacity:.6;font-size:.85em;margin-top:2rem}}</style></head>\n<body>\n<h1>{crumbs}</h1>\n{play_all}<ul>\n{list}</ul>\n\
<footer>Hosted on <a href=\"https://c0mpute.com\">c0mpute</a>. This folder has no index.html, so the gateway lists it.</footer>\n</body></html>\n",
        t = esc(&title)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(size: u64, ct: &str) -> FileEntry {
        FileEntry { size, content_type: ct.into(), chunks: vec!["00".repeat(32)] }
    }

    fn files() -> BTreeMap<String, FileEntry> {
        let mut m = BTreeMap::new();
        m.insert("After the Winter/001. Frost.mp3".into(), entry(5_918_737, "audio/mpeg"));
        m.insert("After the Winter/playlist.m3u".into(), entry(587, "application/octet-stream"));
        m.insert("Ravens/001 <Huginn>.mp3".into(), entry(10, "audio/mpeg"));
        m.insert("Ravens/deep/x.mp4".into(), entry(10, "video/mp4"));
        m.insert("cover.jpg".into(), entry(10, "image/jpeg"));
        m
    }

    #[test]
    fn rows() {
        let f = files();
        assert_eq!(
            rows_under("", f.iter()),
            vec![
                Row::Dir("After the Winter".into()),
                Row::Dir("Ravens".into()),
                Row::File("cover.jpg".into(), 10, "image/jpeg".into())
            ]
        );
        assert_eq!(
            rows_under("Ravens/", f.range("Ravens/".to_string()..)),
            vec![Row::Dir("deep".into()), Row::File("001 <Huginn>.mp3".into(), 10, "audio/mpeg".into())]
        );
        assert!(rows_under("Nope/", f.range("Nope/".to_string()..)).is_empty());
    }

    #[test]
    fn page_escapes_and_links() {
        let f = files();
        let html = render("brongva", "Ravens/", &rows_under("Ravens/", f.range("Ravens/".to_string()..)));
        assert!(html.contains("href=\"001%20%3CHuginn%3E.mp3\""), "{html}");
        assert!(html.contains("001 &lt;Huginn&gt;.mp3"));
        assert!(!html.contains("<Huginn>"));
        assert!(html.contains("<audio controls preload=\"none\" src=\"001%20%3CHuginn%3E.mp3\">"));
        assert!(html.contains("href=\"deep/\""));
        assert!(html.contains("<a href=\"../\">brongva</a> / <a href=\"./\">Ravens</a>"), "{html}");
        let root = render("brongva", "", &rows_under("", f.iter()));
        assert!(root.contains("href=\"After%20the%20Winter/\""));
        assert!(root.contains("<img loading=\"lazy\""));
        let album = render("brongva", "After the Winter/", &rows_under("After the Winter/", f.range("After the Winter/".to_string()..)));
        assert!(!album.contains("Play all"), "one track: no play-all");
        assert!(album.contains("5.9 MB"));
    }

    #[test]
    fn decoding() {
        assert_eq!(decode("/"), "");
        assert_eq!(decode(""), "");
        assert_eq!(decode("/After%20the%20Winter/"), "After the Winter/");
        assert_eq!(decode("/After%20the%20Winter"), "After the Winter");
        assert_eq!(decode("/a/b/?x=1"), "a/b/");
        assert_eq!(encode_segment("After the Winter"), "After%20the%20Winter");
    }
}
