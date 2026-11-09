//! `podcasts`: subscribe to any RSS podcast feed or any c0mpute-hosted show,
//! list episodes, download and play them.
//!
//! * A c0mpute show (`<site_id>`) is read through a gateway at
//!   `/s/<site_id>/feed.xml` (the user's own gateway, `127.0.0.1:7777` by
//!   default), or, with `--p2p`, through a hosting client this CLI runs
//!   itself on a buyer identity.
//! * Anything else is fetched with the same outbound guard workers use
//!   ([`crate::net_guard`]): every hop of every redirect must resolve to
//!   public addresses only, and the connection is pinned to them. Only the
//!   configured gateway's own origin is exempt (it is the user's choice).
//! * Subscriptions persist in `<data_dir>/podcasts/subscriptions.json`.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::rss::{self, Episode, Feed, MAX_FEED_BYTES};
use crate::net_guard;

/// Largest episode file the client will download.
pub const MAX_EPISODE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_REDIRECTS: usize = 8;
const USER_AGENT: &str = concat!("c0mpute-podcasts/", env!("CARGO_PKG_VERSION"));
pub const DEFAULT_GATEWAY: &str = "http://127.0.0.1:7777";

/// Where a subscription's feed comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Source {
    /// A plain RSS URL.
    Url(String),
    /// A c0mpute-hosted show: its hosting site id.
    Site(String),
}

fn is_site_id(s: &str) -> bool {
    s.len() == 20 && s.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

impl Source {
    /// `<site_id>`, `c0mpute://site/<site_id>`, or an http(s) URL.
    pub fn parse(s: &str) -> Result<Source> {
        let s = s.trim();
        if is_site_id(s) {
            return Ok(Source::Site(s.to_string()));
        }
        if let Some(rest) = s.strip_prefix("c0mpute://site/") {
            let id = rest.trim_end_matches('/');
            if is_site_id(id) {
                return Ok(Source::Site(id.to_string()));
            }
            bail!("not a site id: {id}");
        }
        let u = url::Url::parse(s).with_context(|| format!("not a URL or c0mpute site id: {s}"))?;
        if !matches!(u.scheme(), "http" | "https") {
            bail!("feeds must be http(s) URLs or c0mpute site ids");
        }
        Ok(Source::Url(u.to_string()))
    }

    pub fn label(&self) -> String {
        match self {
            Source::Url(u) => u.clone(),
            Source::Site(id) => format!("c0mpute://site/{id}"),
        }
    }
}

/// One subscription, with the last copy of its feed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub source: Source,
    pub title: String,
    pub added_at: i64,
    #[serde(default)]
    pub fetched_at: Option<i64>,
    #[serde(default)]
    pub feed_url: Option<String>,
    #[serde(default)]
    pub feed: Option<Feed>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    v: u8,
    subscriptions: Vec<Subscription>,
}

/// `subscriptions.json`.
pub struct Store {
    path: PathBuf,
    pub subs: Vec<Subscription>,
}

pub fn podcasts_dir() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("podcasts")
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Lowercase `a-z0-9-` slug, at most 48 characters.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "show".into() } else { out }
}

impl Store {
    pub fn open_default() -> Result<Self> {
        Self::open(podcasts_dir().join("subscriptions.json"))
    }

    pub fn open(path: PathBuf) -> Result<Self> {
        let subs = if path.exists() {
            let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            let f: StoreFile = serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
            f.subscriptions
        } else {
            vec![]
        };
        Ok(Store { path, subs })
    }

    /// Write atomically (temp file + rename).
    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(&StoreFile {
            v: 1,
            subscriptions: self.subs.clone(),
        })?;
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Add a subscription (or refresh the one with the same source) and
    /// return its id.
    pub fn upsert(&mut self, source: Source, feed_url: String, feed: Feed) -> String {
        let now = now_unix();
        if let Some(s) = self.subs.iter_mut().find(|s| s.source == source) {
            s.title = feed.title.clone();
            s.feed_url = Some(feed_url);
            s.feed = Some(feed);
            s.fetched_at = Some(now);
            return s.id.clone();
        }
        let base = match &source {
            Source::Site(id) if feed.title.trim().is_empty() => id.clone(),
            _ => slug(&feed.title),
        };
        let mut id = base.clone();
        let mut n = 2;
        while self.subs.iter().any(|s| s.id == id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        self.subs.push(Subscription {
            id: id.clone(),
            source,
            title: feed.title.clone(),
            added_at: now,
            fetched_at: Some(now),
            feed_url: Some(feed_url),
            feed: Some(feed),
        });
        id
    }

    /// Find a show by id, 1-based position in `ls`, site id, or a unique
    /// case-insensitive title fragment.
    pub fn find(&self, q: &str) -> Result<usize> {
        let q = q.trim();
        if let Some(i) = self.subs.iter().position(|s| s.id == q) {
            return Ok(i);
        }
        if let Ok(n) = q.parse::<usize>()
            && n >= 1
            && n <= self.subs.len()
        {
            return Ok(n - 1);
        }
        if let Some(i) = self.subs.iter().position(|s| s.source == Source::Site(q.to_string())) {
            return Ok(i);
        }
        let lower = q.to_lowercase();
        let hits: Vec<usize> = (0..self.subs.len())
            .filter(|&i| self.subs[i].title.to_lowercase().contains(&lower))
            .collect();
        match hits.len() {
            1 => Ok(hits[0]),
            0 => bail!("no subscription matches {q:?} (see `c0mpute podcasts ls`)"),
            _ => bail!(
                "{q:?} matches {} shows: {}",
                hits.len(),
                hits.iter().map(|&i| self.subs[i].id.as_str()).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    pub fn remove(&mut self, q: &str) -> Result<Subscription> {
        let i = self.find(q)?;
        Ok(self.subs.remove(i))
    }
}

/// Episodes newest first (feed order where dates are missing).
pub fn sorted_episodes(feed: &Feed) -> Vec<&Episode> {
    let mut v: Vec<&Episode> = feed.episodes.iter().collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.published.unwrap_or(i64::MIN)));
    v
}

/// Pick an episode: none = newest; a 1-based number in newest-first order;
/// an exact guid; or a case-insensitive title fragment (newest match).
pub fn find_episode<'a>(feed: &'a Feed, q: Option<&str>) -> Result<&'a Episode> {
    let eps = sorted_episodes(feed);
    let Some(q) = q.map(str::trim).filter(|q| !q.is_empty()) else {
        return eps.first().copied().context("the feed has no episodes");
    };
    if let Ok(n) = q.parse::<usize>()
        && n >= 1
        && n <= eps.len()
    {
        return Ok(eps[n - 1]);
    }
    if let Some(e) = eps.iter().find(|e| e.guid.as_deref() == Some(q)) {
        return Ok(e);
    }
    let lower = q.to_lowercase();
    eps.into_iter()
        .find(|e| e.title.to_lowercase().contains(&lower))
        .with_context(|| format!("no episode matches {q:?}"))
}

/// Reads feeds and episode files.
#[derive(Clone)]
pub struct Client {
    gateway: url::Url,
    allow: fn(IpAddr) -> bool,
}

impl Client {
    /// `gateway` serves c0mpute shows; it is trusted (no address guard).
    pub fn new(gateway: &str) -> Result<Self> {
        let mut g = url::Url::parse(gateway.trim()).with_context(|| format!("gateway URL {gateway:?}"))?;
        if !matches!(g.scheme(), "http" | "https") {
            bail!("gateway must be an http(s) URL");
        }
        if !g.path().ends_with('/') {
            let p = format!("{}/", g.path());
            g.set_path(&p);
        }
        Ok(Client {
            gateway: g,
            allow: net_guard::is_public_ip,
        })
    }

    /// Replace the address check for non-gateway URLs (tests use loopback).
    pub fn with_allow(mut self, allow: fn(IpAddr) -> bool) -> Self {
        self.allow = allow;
        self
    }

    /// The URL a source's feed is read from.
    pub fn feed_url(&self, src: &Source) -> Result<url::Url> {
        match src {
            Source::Url(u) => Ok(url::Url::parse(u)?),
            Source::Site(id) => Ok(self.gateway.join(&format!("s/{id}/feed.xml"))?),
        }
    }

    fn trusted(&self, u: &url::Url) -> bool {
        u.origin() == self.gateway.origin()
    }

    /// GET with manual redirects; every hop outside the gateway's origin is
    /// vetted and pinned to public addresses.
    async fn get(&self, start: url::Url, total: Option<Duration>) -> Result<(url::Url, reqwest::Response)> {
        let mut url = start;
        for _ in 0..=MAX_REDIRECTS {
            if !matches!(url.scheme(), "http" | "https") {
                bail!("refusing {} URL", url.scheme());
            }
            let mut b = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(15))
                .read_timeout(Duration::from_secs(60));
            if let Some(t) = total {
                b = b.timeout(t);
            }
            if !self.trusted(&url) {
                let target = net_guard::vet_parsed(url.clone(), self.allow).await?;
                if let Some(h) = &target.pin_host {
                    b = b.resolve_to_addrs(h, &target.addrs);
                }
            }
            let resp = b
                .build()?
                .get(url.clone())
                .send()
                .await
                .with_context(|| format!("GET {url}"))?;
            if resp.status().is_redirection() {
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .with_context(|| format!("{url}: redirect without a Location"))?;
                url = url.join(loc).with_context(|| format!("bad redirect {loc:?}"))?;
                continue;
            }
            if !resp.status().is_success() {
                bail!("GET {url}: HTTP {}", resp.status());
            }
            return Ok((url, resp));
        }
        bail!("too many redirects")
    }

    /// Fetch and parse a feed. Returns the final URL (after redirects), which
    /// relative enclosure URLs resolve against.
    pub async fn fetch_feed(&self, src: &Source) -> Result<(url::Url, Feed)> {
        let (final_url, mut resp) = self.get(self.feed_url(src)?, Some(Duration::from_secs(60))).await?;
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_FEED_BYTES {
                bail!("feed is over {MAX_FEED_BYTES} bytes");
            }
        }
        let text = String::from_utf8_lossy(&body);
        let feed = rss::parse(&text).with_context(|| format!("read feed {final_url}"))?;
        Ok((final_url, feed))
    }

    /// Download an episode's enclosure into `dir`. Skips the download when a
    /// file of the advertised length is already there.
    pub async fn download(&self, feed_url: &url::Url, ep: &Episode, dir: &Path) -> Result<PathBuf> {
        let enc = ep.enclosure.as_ref().context("episode has no audio enclosure")?;
        let url = feed_url
            .join(&enc.url)
            .with_context(|| format!("bad enclosure URL {:?}", enc.url))?;
        std::fs::create_dir_all(dir)?;
        let dest = dir.join(episode_file_name(ep, &url));
        if let (Ok(m), Some(len)) = (std::fs::metadata(&dest), enc.length)
            && len > 0
            && m.len() == len
        {
            return Ok(dest);
        }
        let (_, mut resp) = self.get(url, None).await?;
        if resp.content_length().is_some_and(|n| n > MAX_EPISODE_BYTES) {
            bail!("episode is over {MAX_EPISODE_BYTES} bytes");
        }
        let part = dest.with_extension("part");
        let mut f = tokio::fs::File::create(&part).await?;
        let mut n: u64 = 0;
        use tokio::io::AsyncWriteExt;
        while let Some(chunk) = resp.chunk().await? {
            n += chunk.len() as u64;
            if n > MAX_EPISODE_BYTES {
                drop(f);
                let _ = tokio::fs::remove_file(&part).await;
                bail!("episode is over {MAX_EPISODE_BYTES} bytes");
            }
            f.write_all(&chunk).await?;
        }
        f.flush().await?;
        drop(f);
        tokio::fs::rename(&part, &dest).await?;
        Ok(dest)
    }
}

/// `<YYYY-MM-DD>-<title-slug>.<ext>` (ext from the URL path, else the MIME type).
pub fn episode_file_name(ep: &Episode, url: &url::Url) -> String {
    let from_path = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .and_then(|f| f.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()))
        .filter(|e| super::show::AUDIO_TYPES.iter().any(|(x, _)| x == e) || e == "mp4" || e == "m4b");
    let ext = from_path.unwrap_or_else(|| {
        let mime = ep.enclosure.as_ref().and_then(|e| e.mime.as_deref()).unwrap_or("");
        super::show::AUDIO_TYPES
            .iter()
            .find(|(_, m)| *m == mime)
            .map(|(e, _)| e.to_string())
            .unwrap_or_else(|| "mp3".into())
    });
    let date = ep
        .published
        .map(|t| format!("{}-", super::dates::fmt_ymd(t)))
        .unwrap_or_default();
    format!("{date}{}.{ext}", slug(&ep.title))
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

/// A local audio player: mpv, else ffplay.
pub fn player() -> Option<(&'static str, Vec<&'static str>)> {
    if on_path("mpv") {
        return Some(("mpv", vec!["--no-video", "--really-quiet"]));
    }
    if on_path("ffplay") {
        return Some(("ffplay", vec!["-nodisp", "-autoexit", "-loglevel", "error"]));
    }
    None
}

/// A hosting client this CLI runs itself (buyer identity, holds nothing),
/// served on an ephemeral loopback port, for reading c0mpute shows without a
/// gateway. Waits until it has learned `site_id`'s record from gossip.
pub struct LocalGateway {
    pub url: String,
    _hosting: crate::hosting::Hosting,
    _supervisor: crate::Supervisor,
}

impl LocalGateway {
    pub async fn start(site_id: &str, wait: Duration) -> Result<Self> {
        let supervisor = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
        let hosting = crate::hosting::Hosting::start(
            supervisor.libp2p.clone(),
            supervisor.store.clone(),
            crate::hosting::Options {
                hosts: false,
                ..Default::default()
            },
        )
        .await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = hosting.router();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let started = std::time::Instant::now();
        loop {
            if hosting.sites().await.iter().any(|(r, _)| r.site_id == site_id) {
                break;
            }
            if started.elapsed() > wait {
                bail!(
                    "no hosting node announced site {site_id} within {}s (records are re-gossiped every few minutes; try a longer --wait-secs or a gateway)",
                    wait.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Ok(LocalGateway {
            url: format!("http://{addr}"),
            _hosting: hosting,
            _supervisor: supervisor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::podcast::rss::Enclosure;

    fn feed(title: &str, n: usize) -> Feed {
        Feed {
            title: title.into(),
            episodes: (0..n)
                .map(|i| Episode {
                    title: format!("Episode {i}"),
                    guid: Some(format!("g{i}")),
                    published: Some(1_000 + i as i64),
                    enclosure: Some(Enclosure {
                        url: format!("e{i}.mp3"),
                        length: Some(10),
                        mime: Some("audio/mpeg".into()),
                    }),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn sources_parse() {
        assert_eq!(
            Source::parse("0123456789abcdef0123").unwrap(),
            Source::Site("0123456789abcdef0123".into())
        );
        assert_eq!(
            Source::parse("c0mpute://site/0123456789abcdef0123/").unwrap(),
            Source::Site("0123456789abcdef0123".into())
        );
        assert!(matches!(Source::parse("https://feeds.example.com/x").unwrap(), Source::Url(_)));
        assert!(Source::parse("file:///etc/passwd").is_err());
        assert!(Source::parse("hello").is_err());
    }

    #[test]
    fn subscription_store_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p/subscriptions.json");
        let mut s = Store::open(path.clone()).unwrap();
        assert!(s.subs.is_empty());
        let a = s.upsert(Source::Url("https://a.example/feed".into()), "https://a.example/feed".into(), feed("Tea & Code", 3));
        let b = s.upsert(Source::Site("0123456789abcdef0123".into()), "http://gw/s/x/feed.xml".into(), feed("Tea & Code", 1));
        assert_eq!(a, "tea-code");
        assert_eq!(b, "tea-code-2", "ids stay unique");
        // Same source again updates in place.
        let again = s.upsert(Source::Url("https://a.example/feed".into()), "https://a.example/feed".into(), feed("Tea and Code", 4));
        assert_eq!(again, "tea-code");
        assert_eq!(s.subs.len(), 2);
        s.save().unwrap();
        assert!(!path.with_extension("json.tmp").exists());

        let s = Store::open(path.clone()).unwrap();
        assert_eq!(s.subs.len(), 2);
        assert_eq!(s.subs[0].title, "Tea and Code");
        assert_eq!(s.subs[0].feed.as_ref().unwrap().episodes.len(), 4);
        assert_eq!(s.find("tea-code-2").unwrap(), 1);
        assert_eq!(s.find("1").unwrap(), 0);
        assert_eq!(s.find("0123456789abcdef0123").unwrap(), 1);
        assert_eq!(s.find("and code").unwrap(), 0);
        assert!(s.find("tea").is_err(), "ambiguous");
        assert!(s.find("nothing").is_err());
        let mut s = s;
        let gone = s.remove("tea-code").unwrap();
        assert_eq!(gone.id, "tea-code");
        s.save().unwrap();
        assert_eq!(Store::open(path).unwrap().subs.len(), 1);
    }

    #[test]
    fn picks_episodes() {
        let f = feed("x", 3);
        assert_eq!(find_episode(&f, None).unwrap().title, "Episode 2", "newest");
        assert_eq!(find_episode(&f, Some("3")).unwrap().title, "Episode 0");
        assert_eq!(find_episode(&f, Some("g1")).unwrap().title, "Episode 1");
        assert_eq!(find_episode(&f, Some("episode 1")).unwrap().title, "Episode 1");
        assert!(find_episode(&f, Some("nope")).is_err());
        assert!(find_episode(&feed("y", 0), None).is_err());
    }

    #[test]
    fn episode_file_names() {
        let mut e = feed("x", 1).episodes.remove(0);
        e.title = "Ep 1: Hello, World!".into();
        e.published = crate::podcast::dates::parse_date_input("2026-06-04");
        let u = url::Url::parse("https://cdn.example/a/b.m4a?x=1").unwrap();
        assert_eq!(episode_file_name(&e, &u), "2026-06-04-ep-1-hello-world.m4a");
        let u = url::Url::parse("https://track.example/redirect").unwrap();
        assert_eq!(episode_file_name(&e, &u), "2026-06-04-ep-1-hello-world.mp3");
    }

    #[tokio::test]
    async fn the_guard_refuses_private_feeds_but_trusts_the_gateway() {
        let c = Client::new("http://127.0.0.1:9").unwrap();
        // Private address, not the gateway: refused before any connection.
        let err = c
            .fetch_feed(&Source::Url("http://10.0.0.1/feed.xml".into()))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("non-public"), "{err:#}");
        // The gateway origin is trusted (connection refused, not vetoed).
        let err = c.fetch_feed(&Source::Site("0123456789abcdef0123".into())).await.unwrap_err();
        assert!(!format!("{err:#}").contains("non-public"), "{err:#}");
        assert_eq!(
            c.feed_url(&Source::Site("0123456789abcdef0123".into())).unwrap().as_str(),
            "http://127.0.0.1:9/s/0123456789abcdef0123/feed.xml"
        );
    }
}
