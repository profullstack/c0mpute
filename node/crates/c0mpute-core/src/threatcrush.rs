//! ThreatCrush on a c0mpute node (`threatcrush` plugin, optional).
//!
//! ThreatCrush (threatcrush.com, `@profullstack/threatcrush`) is a host
//! security daemon: it tails web and auth logs, scores each request against
//! attack rules (OWASP CRS, scanners, brute force) and bans repeat offenders
//! in nftables. A c0mpute gateway answers the public internet on :7777 but
//! wrote no access log, so ThreatCrush could not see it.
//!
//! With `[threatcrush] enabled = true` the gateway writes one line per request
//! in nginx's `vhost` format (the format ThreatCrush already parses: Host,
//! client address, request, status, bytes, referer, user agent) to
//! `<data_dir>/gateway/access.log`, rotating at `max_log_mib` (one `.1` kept).
//! `c0mpute plugin install threatcrush` installs the daemon and registers this
//! file with it (`/etc/threatcrush/threatcrushd.conf.d/c0mpute.conf`).
//!
//! The client address is the TCP peer, unless the peer is a trusted proxy
//! (loopback and private ranges by default: the Moshpit edge on the same
//! box), in which case X-Real-IP / the last X-Forwarded-For hop is used.
//! Without that a ban on "the client" would ban your own reverse proxy.

use std::{
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    sync::{mpsc, Arc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{ConnectInfo, Request, State},
    middleware::Next,
    response::Response,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// `[threatcrush]` in config.toml.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreatcrushConfig {
    /// Write the gateway access log ThreatCrush tails. Default false.
    #[serde(default)]
    pub enabled: bool,
    /// Default `<data_dir>/gateway/access.log`.
    #[serde(default)]
    pub access_log: Option<PathBuf>,
    /// Peers whose X-Real-IP / X-Forwarded-For is believed (CIDRs).
    #[serde(default = "default_trusted")]
    pub trusted_proxies: Vec<String>,
    /// Rotate the log at this size (one `.1` is kept).
    #[serde(default = "default_max_mib")]
    pub max_log_mib: u64,
}

fn default_trusted() -> Vec<String> {
    [
        "127.0.0.0/8",
        "::1/128",
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "fc00::/7",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
fn default_max_mib() -> u64 {
    256
}

impl Default for ThreatcrushConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            access_log: None,
            trusted_proxies: default_trusted(),
            max_log_mib: default_max_mib(),
        }
    }
}

impl ThreatcrushConfig {
    /// `C0MPUTE_THREATCRUSH=1|0` overrides `enabled`.
    pub fn is_enabled(&self) -> bool {
        match std::env::var("C0MPUTE_THREATCRUSH").ok().as_deref() {
            Some("1") | Some("true") => true,
            Some("0") | Some("false") => false,
            _ => self.enabled,
        }
    }

    pub fn log_path(&self) -> PathBuf {
        self.access_log.clone().unwrap_or_else(default_log_path)
    }
}

/// Where the gateway access log goes when none is configured.
pub fn default_log_path() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gateway")
        .join("access.log")
}

/// Handle the middleware writes through. Lines go to a writer thread over a
/// bounded channel; a full channel drops the line rather than slow a request.
#[derive(Clone)]
pub struct AccessLog {
    tx: mpsc::SyncSender<String>,
    trusted: Arc<Vec<Net>>,
}

impl AccessLog {
    /// Start the writer, or None when ThreatCrush is off for this node.
    pub fn from_config(cfg: &crate::config::Config) -> Option<AccessLog> {
        let t = &cfg.threatcrush;
        if !t.is_enabled() {
            return None;
        }
        let path = t.log_path();
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                warn!(err = %e, path = %path.display(), "threatcrush: no access log");
                return None;
            }
        }
        let (tx, rx) = mpsc::sync_channel::<String>(8192);
        let max = t.max_log_mib.max(1) * 1024 * 1024;
        let p = path.clone();
        std::thread::Builder::new()
            .name("gateway-access-log".into())
            .spawn(move || writer(p, max, rx))
            .ok()?;
        info!(path = %path.display(), "threatcrush: gateway access log on");
        Some(AccessLog {
            tx,
            trusted: Arc::new(compile(&t.trusted_proxies)),
        })
    }

    fn client(&self, peer: Option<SocketAddr>, h: &axum::http::HeaderMap) -> String {
        let peer_ip = peer.map(|p| p.ip().to_canonical());
        match peer_ip {
            Some(ip) if !self.trusted.iter().any(|n| n.contains(ip)) => ip.to_string(),
            other => {
                let fwd = x402_gateway::client_ip(h);
                if fwd.is_empty() {
                    other.map(|i| i.to_string()).unwrap_or_else(|| "-".into())
                } else {
                    fwd
                }
            }
        }
    }
}

/// A CIDR, v4 or v6 (the x402 port's matcher is v4 only).
#[derive(Clone, Debug)]
pub struct Net {
    v6: bool,
    base: u128,
    mask: u128,
}

impl Net {
    pub fn parse(s: &str) -> Option<Net> {
        let s = s.trim();
        let (ip, len) = match s.split_once('/') {
            Some((ip, l)) => (ip, Some(l.parse::<u32>().ok()?)),
            None => (s, None),
        };
        let ip: std::net::IpAddr = ip.parse().ok()?;
        let (v6, bits, width) = match ip.to_canonical() {
            std::net::IpAddr::V4(a) => (false, u32::from(a) as u128, 32u32),
            std::net::IpAddr::V6(a) => (true, u128::from(a), 128u32),
        };
        let len = len.unwrap_or(width);
        if len > width {
            return None;
        }
        let mask = if len == 0 { 0 } else { (!0u128) << (width - len) & width_mask(width) };
        Some(Net { v6, base: bits & mask, mask })
    }

    pub fn contains(&self, ip: std::net::IpAddr) -> bool {
        match ip.to_canonical() {
            std::net::IpAddr::V4(a) if !self.v6 => (u32::from(a) as u128) & self.mask == self.base,
            std::net::IpAddr::V6(a) if self.v6 => u128::from(a) & self.mask == self.base,
            _ => false,
        }
    }
}

fn width_mask(width: u32) -> u128 {
    if width == 128 { !0 } else { (1u128 << width) - 1 }
}

pub fn compile(list: &[String]) -> Vec<Net> {
    list.iter().filter_map(|s| Net::parse(s)).collect()
}

/// axum middleware: `app.layer(from_fn_with_state(log, threatcrush::log))`.
pub async fn log(State(l): State<AccessLog>, req: Request, next: Next) -> Response {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let h = req.headers();
    let host = hv(h, "host");
    let client = l.client(peer, h);
    let referer = hv(h, "referer");
    let ua = hv(h, "user-agent");
    let target = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let request = format!("{} {} {:?}", req.method(), target, req.version());
    let resp = next.run(req).await;
    let bytes = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("0")
        .to_string();
    let line = format_line(
        &host,
        &client,
        SystemTime::now(),
        &request,
        resp.status().as_u16(),
        &bytes,
        &referer,
        &ua,
    );
    let _ = l.tx.try_send(line);
    resp
}

fn hv(h: &axum::http::HeaderMap, name: &str) -> String {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// One nginx `vhost` line:
/// `$host $remote_addr - $remote_user [$time_local] "$request" $status
/// $body_bytes_sent "$http_referer" "$http_user_agent"`.
#[allow(clippy::too_many_arguments)]
pub fn format_line(
    host: &str,
    client: &str,
    at: SystemTime,
    request: &str,
    status: u16,
    bytes: &str,
    referer: &str,
    ua: &str,
) -> String {
    let host = if host.is_empty() { "-" } else { host };
    let referer = if referer.is_empty() { "-" } else { referer };
    let ua = if ua.is_empty() { "-" } else { ua };
    format!(
        "{} {} - - [{}] \"{}\" {} {} \"{}\" \"{}\"",
        esc(host).replace(' ', "_"),
        esc(client),
        nginx_time(at),
        esc(request),
        status,
        bytes,
        esc(referer),
        esc(ua),
    )
}

/// nginx escapes `"`, `\` and control bytes as `\xHH` in its logs.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' | '\\' => out.push_str(&format!("\\x{:02X}", c as u32)),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `10/Nov/2026:13:55:36 +0000` (UTC).
pub fn nginx_time(at: SystemTime) -> String {
    let secs = at.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{:02}/{}/{}:{:02}:{:02}:{:02} +0000",
        d,
        MON[(m - 1) as usize],
        y,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days -> (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn open(path: &PathBuf) -> Option<(std::io::BufWriter<std::fs::File>, u64)> {
    let f = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    Some((std::io::BufWriter::new(f), len))
}

fn writer(path: PathBuf, max: u64, rx: mpsc::Receiver<String>) {
    let Some((mut w, mut len)) = open(&path) else {
        warn!(path = %path.display(), "threatcrush: cannot open access log");
        return;
    };
    loop {
        let line = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(l) => l,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = w.flush();
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = w.flush();
                return;
            }
        };
        if writeln!(w, "{line}").is_ok() {
            len += line.len() as u64 + 1;
        }
        // Drain whatever else is waiting, then flush once.
        while let Ok(l) = rx.try_recv() {
            if writeln!(w, "{l}").is_ok() {
                len += l.len() as u64 + 1;
            }
        }
        let _ = w.flush();
        if len >= max {
            let _ = w.flush();
            let mut rotated = path.clone().into_os_string();
            rotated.push(".1");
            let _ = std::fs::rename(&path, PathBuf::from(rotated));
            match open(&path) {
                Some((nw, nl)) => {
                    w = nw;
                    len = nl;
                }
                None => {
                    warn!(path = %path.display(), "threatcrush: cannot reopen access log");
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_is_nginx_shaped() {
        // 2026-11-10T13:55:36Z
        let t = UNIX_EPOCH + Duration::from_secs(1_794_318_936);
        assert_eq!(nginx_time(t), "10/Nov/2026:13:55:36 +0000");
        assert_eq!(nginx_time(UNIX_EPOCH), "01/Jan/1970:00:00:00 +0000");
        // leap day
        let t = UNIX_EPOCH + Duration::from_secs(1_709_164_800);
        assert_eq!(nginx_time(t), "29/Feb/2024:00:00:00 +0000");
    }

    #[test]
    fn line_matches_vhost_format() {
        let t = UNIX_EPOCH + Duration::from_secs(1_794_318_936);
        let l = format_line(
            "gw.example:7777",
            "45.33.22.11",
            t,
            "GET /s/53021d6e8d5a0e1aac48/?q=\"x\" HTTP/1.1",
            402,
            "614",
            "",
            "GPTBot/1.2",
        );
        assert_eq!(
            l,
            "gw.example:7777 45.33.22.11 - - [10/Nov/2026:13:55:36 +0000] \
\"GET /s/53021d6e8d5a0e1aac48/?q=\\x22x\\x22 HTTP/1.1\" 402 614 \"-\" \"GPTBot/1.2\""
        );
    }

    fn log_for(trusted: &[&str]) -> AccessLog {
        let (tx, _rx) = mpsc::sync_channel(1);
        AccessLog {
            tx,
            trusted: Arc::new(compile(
                &trusted.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )),
        }
    }

    #[test]
    fn forwarded_for_only_from_trusted_peers() {
        let l = log_for(&["127.0.0.0/8"]);
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-real-ip", "203.0.113.9".parse().unwrap());
        // the edge on loopback forwards the real client
        assert_eq!(l.client(Some("127.0.0.1:5000".parse().unwrap()), &h), "203.0.113.9");
        // anyone else claiming a header is logged as themselves
        assert_eq!(l.client(Some("198.51.100.7:5000".parse().unwrap()), &h), "198.51.100.7");
        // v4-mapped v6 peers are canonicalised
        assert_eq!(
            l.client(Some("[::ffff:198.51.100.7]:5000".parse().unwrap()), &h),
            "198.51.100.7"
        );
    }

    #[test]
    fn nets_match_v4_and_v6() {
        let n = compile(&default_trusted());
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        assert!(n.iter().any(|x| x.contains(ip("127.0.0.1"))));
        assert!(n.iter().any(|x| x.contains(ip("::1"))));
        assert!(n.iter().any(|x| x.contains(ip("fd12::5"))));
        assert!(n.iter().any(|x| x.contains(ip("172.20.0.3"))));
        assert!(!n.iter().any(|x| x.contains(ip("172.32.0.1"))));
        assert!(!n.iter().any(|x| x.contains(ip("2a0e:97c0:3ea:b8::1"))));
        assert!(!n.iter().any(|x| x.contains(ip("8.8.8.8"))));
        assert!(Net::parse("0.0.0.0/0").unwrap().contains(ip("8.8.8.8")));
        assert!(Net::parse("10.0.0.0/33").is_none());
    }

    #[test]
    fn config_defaults_off() {
        let c: ThreatcrushConfig = toml::from_str("").unwrap();
        assert!(!c.enabled);
        assert_eq!(c.max_log_mib, 256);
        assert!(c.trusted_proxies.iter().any(|p| p == "127.0.0.0/8"));
    }

    #[test]
    fn writer_rotates() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("access.log");
        let (tx, rx) = mpsc::sync_channel(16);
        let pp = p.clone();
        let h = std::thread::spawn(move || writer(pp, 10, rx));
        tx.send("first line here".into()).unwrap();
        // let the writer rotate before the second line arrives
        std::thread::sleep(Duration::from_millis(200));
        tx.send("second".into()).unwrap();
        drop(tx);
        h.join().unwrap();
        let mut rotated = p.clone().into_os_string();
        rotated.push(".1");
        assert_eq!(std::fs::read_to_string(PathBuf::from(rotated)).unwrap(), "first line here\n");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "second\n");
    }
}
