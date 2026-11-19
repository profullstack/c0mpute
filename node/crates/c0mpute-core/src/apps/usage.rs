//! Metering for apps: what each app used on the node that runs it (CPU, RAM,
//! disk, bandwidth), hour by hour, priced at that node's rates.
//!
//! * The primary samples its app's container about once a minute (`docker
//!   stats` for CPU, memory and network; the writable layer plus the state
//!   volume for disk) into a [`Meter`].
//! * When an hour ends the meter closes it into a [`UsageReport`] and the node
//!   gossips it on the apps topic. Gossip is signed by the sending peer, and
//!   a report is only kept when its `holder` is that peer, so every report is
//!   attributable to the node that metered it. It is the holder's own
//!   account of the hour, not a proof: the owner sees it, and the rates, on
//!   every gateway (`GET /apps/v1/usage`).
//! * Price: CPU-hours, GiB-hours of RAM, GiB-months of disk (prorated) and
//!   GiB sent out, at the holder's `[apps] price_*` rates ([`Rates`]).
//!   Inbound traffic is free. Nothing is paid from this yet; it is the
//!   ledger the dashboard shows and settlement will pay from.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::payments::Micros;

const GIB: f64 = 1_073_741_824.0;
pub const HOUR_SECS: u64 = 3600;
/// A sample gap longer than this is counted as this long: a node that slept
/// or lost the container does not bill the whole gap.
pub const MAX_SAMPLE_GAP_SECS: u64 = 180;
/// Hours of reports a node keeps from gossip.
pub const KEEP_HOURS: u64 = 24;
/// Reports kept at most (every app, holder and hour).
pub const MAX_REPORTS: usize = 100_000;
/// Reports per gossip message.
pub const REPORTS_PER_MESSAGE: usize = 100;

/// What a node charges for running apps, micro-USD.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rates {
    pub cpu_hour: Micros,
    pub ram_gib_hour: Micros,
    pub disk_gib_month: Micros,
    pub egress_gib: Micros,
}

impl Default for Rates {
    /// House defaults: $0.01 per CPU-hour, $0.002 per GiB-hour of RAM,
    /// $0.004 per GiB-month of disk (the hosting default), $0.01 per GiB out.
    fn default() -> Self {
        Rates { cpu_hour: 10_000, ram_gib_hour: 2_000, disk_gib_month: 4_000, egress_gib: 10_000 }
    }
}

impl Rates {
    /// From `[apps] price_*` strings ("0.01"); a blank or bad one keeps the default.
    pub fn from_config(cpu: &str, ram: &str, disk: &str, egress: &str) -> Rates {
        let d = Rates::default();
        let p = |s: &str, def: Micros| {
            if s.trim().is_empty() {
                return def;
            }
            match crate::payments::parse_usd(s) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(value = %s, err = %e, "apps: bad price in [apps]; using the default");
                    def
                }
            }
        };
        Rates {
            cpu_hour: p(cpu, d.cpu_hour),
            ram_gib_hour: p(ram, d.ram_gib_hour),
            disk_gib_month: p(disk, d.disk_gib_month),
            egress_gib: p(egress, d.egress_gib),
        }
    }
}

/// One app's use of one node over one hour.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    pub app_id: String,
    pub owner: String,
    /// The node that ran it (base58 peer id).
    pub holder: String,
    /// Start of the hour, unix seconds.
    pub hour: u64,
    /// Seconds the hour was metered (the app ran).
    pub secs: u64,
    pub cpu_s: f64,
    pub ram_gib_h: f64,
    pub disk_gib_h: f64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rates: Rates,
    /// [`price`] of the above at `rates`.
    pub amount: Micros,
}

/// What `rep` costs at its rates, micro-USD (rounded down).
pub fn price(rep: &UsageReport) -> Micros {
    let r = &rep.rates;
    let usd_micros = rep.cpu_s / 3600.0 * r.cpu_hour as f64
        + rep.ram_gib_h * r.ram_gib_hour as f64
        + rep.disk_gib_h / (30.0 * 24.0) * r.disk_gib_month as f64
        + rep.tx_bytes as f64 / GIB * r.egress_gib as f64;
    if usd_micros.is_finite() && usd_micros > 0.0 { usd_micros.min(1e15) as Micros } else { 0 }
}

/// The parts of a report a node can sanity-check: finite, non-negative, no
/// more than the hour allows (64 CPUs, 1 TiB of RAM, 64 TiB of disk).
pub fn plausible(r: &UsageReport, now: u64) -> bool {
    let ok = |v: f64, max: f64| v.is_finite() && (0.0..=max).contains(&v);
    r.hour % HOUR_SECS == 0
        && r.hour + HOUR_SECS <= now + 300
        && r.hour + KEEP_HOURS * HOUR_SECS >= now
        && r.secs <= HOUR_SECS
        && ok(r.cpu_s, 64.0 * 3600.0)
        && ok(r.ram_gib_h, 1024.0)
        && ok(r.disk_gib_h, 64.0 * 1024.0)
        && r.app_id.len() <= 64
        && r.owner.len() <= 64
}

// ── docker stats ─────────────────────────────────────────────────────────────

/// One `docker stats --no-stream --format '{{json .}}'` line, the parts used.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    /// CPUs in use (1.0 = one full CPU).
    pub cpus: f64,
    pub mem_bytes: u64,
    /// Since the container started.
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// "12.3MiB", "1.5kB", "648B", "2GB" -> bytes. Docker uses binary units for
/// memory and decimal for network.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.trim().parse().ok()?;
    let mul = match unit.trim() {
        "" | "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => GIB,
        "TiB" => GIB * 1024.0,
        _ => return None,
    };
    let v = n * mul;
    (v.is_finite() && v >= 0.0).then_some(v as u64)
}

pub fn parse_stats(line: &str) -> Option<Stats> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let cpu: f64 = v["CPUPerc"].as_str()?.trim().trim_end_matches('%').parse().ok()?;
    let mem = parse_size(v["MemUsage"].as_str()?.split('/').next()?)?;
    let mut net = v["NetIO"].as_str()?.split('/');
    let rx = parse_size(net.next()?)?;
    let tx = parse_size(net.next()?)?;
    Some(Stats { cpus: (cpu / 100.0).max(0.0), mem_bytes: mem, rx_bytes: rx, tx_bytes: tx })
}

// ── the meter ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Acc {
    owner: String,
    hour: u64,
    secs: u64,
    cpu_s: f64,
    ram_gib_s: f64,
    disk_gib_s: f64,
    rx: u64,
    tx: u64,
    last_at: u64,
    last_rx: u64,
    last_tx: u64,
}

impl Acc {
    fn new(owner: &str, hour: u64, now: u64, s: Stats) -> Acc {
        Acc {
            owner: owner.to_string(),
            hour,
            secs: 0,
            cpu_s: 0.0,
            ram_gib_s: 0.0,
            disk_gib_s: 0.0,
            rx: 0,
            tx: 0,
            last_at: now,
            last_rx: s.rx_bytes,
            last_tx: s.tx_bytes,
        }
    }

    /// Count `dt` seconds at `s`; with `traffic`, the counters' growth too
    /// (a counter that fell means the container restarted: all of it is new).
    fn add(&mut self, s: Stats, disk_bytes: u64, dt: u64, traffic: bool) {
        let d = dt as f64;
        self.secs = (self.secs + dt).min(HOUR_SECS);
        self.cpu_s += s.cpus * d;
        self.ram_gib_s += s.mem_bytes as f64 / GIB * d;
        self.disk_gib_s += disk_bytes as f64 / GIB * d;
        if traffic {
            self.rx += if s.rx_bytes >= self.last_rx { s.rx_bytes - self.last_rx } else { s.rx_bytes };
            self.tx += if s.tx_bytes >= self.last_tx { s.tx_bytes - self.last_tx } else { s.tx_bytes };
            self.last_rx = s.rx_bytes;
            self.last_tx = s.tx_bytes;
        }
    }
}

/// Running totals per app for the hour in progress, on the node that runs them.
#[derive(Debug, Default)]
pub struct Meter {
    apps: HashMap<String, Acc>,
    done: Vec<UsageReport>,
}

impl Meter {
    /// Add one sample of `app_id` taken at `now` (unix seconds): the time
    /// since the previous sample is counted at this sample's rates (at most
    /// [`MAX_SAMPLE_GAP_SECS`]), split at an hour boundary. The first sample
    /// of an app only sets the baseline.
    #[allow(clippy::too_many_arguments)]
    pub fn sample(&mut self, app_id: &str, owner: &str, holder: &str, rates: Rates, s: Stats, disk_bytes: u64, now: u64) {
        let hour = now / HOUR_SECS * HOUR_SECS;
        let Some(a) = self.apps.get_mut(app_id) else {
            self.apps.insert(app_id.to_string(), Acc::new(owner, hour, now, s));
            return;
        };
        if now <= a.last_at {
            return;
        }
        if a.hour != hour {
            // The rest of the old hour (and all traffic since) goes to it.
            let end = a.hour + HOUR_SECS;
            let gap = now - a.last_at;
            let old_part = end.saturating_sub(a.last_at).min(gap).min(MAX_SAMPLE_GAP_SECS);
            a.add(s, disk_bytes, old_part, true);
            let new_part = if a.hour + HOUR_SECS == hour { now - hour } else { 0 };
            let new_part = new_part.min(MAX_SAMPLE_GAP_SECS.saturating_sub(old_part));
            let old = self.apps.remove(app_id).expect("present");
            self.close(app_id, holder, rates, old);
            let mut n = Acc::new(owner, hour, now, s);
            n.add(s, disk_bytes, new_part, false);
            self.apps.insert(app_id.to_string(), n);
            return;
        }
        let dt = (now - a.last_at).min(MAX_SAMPLE_GAP_SECS);
        a.add(s, disk_bytes, dt, true);
        a.last_at = now;
        a.owner = owner.to_string();
    }

    fn close(&mut self, app_id: &str, holder: &str, rates: Rates, a: Acc) {
        if a.secs == 0 && a.rx == 0 && a.tx == 0 {
            return;
        }
        let mut r = UsageReport {
            app_id: app_id.to_string(),
            owner: a.owner,
            holder: holder.to_string(),
            hour: a.hour,
            secs: a.secs,
            cpu_s: a.cpu_s,
            ram_gib_h: a.ram_gib_s / 3600.0,
            disk_gib_h: a.disk_gib_s / 3600.0,
            rx_bytes: a.rx,
            tx_bytes: a.tx,
            rates,
            amount: 0,
        };
        r.amount = price(&r);
        self.done.push(r);
    }

    /// Reports for every hour that has ended by `now`, including apps no
    /// longer sampled (stopped, or handed to another node).
    pub fn take_finished(&mut self, holder: &str, rates: Rates, now: u64) -> Vec<UsageReport> {
        let hour = now / HOUR_SECS * HOUR_SECS;
        let ended: Vec<String> = self.apps.iter().filter(|(_, a)| a.hour < hour).map(|(k, _)| k.clone()).collect();
        for id in ended {
            let a = self.apps.remove(&id).expect("present");
            self.close(&id, holder, rates, a);
        }
        std::mem::take(&mut self.done)
    }
}

// ── reports heard on gossip ──────────────────────────────────────────────────

/// Every report this node has heard (and its own), newest [`KEEP_HOURS`].
#[derive(Debug, Default)]
pub struct Book {
    reports: HashMap<(String, String, u64), UsageReport>,
}

impl Book {
    /// Keep `r` heard from `source` at `now`. Only the holder speaks for its
    /// own hours; the amount is recomputed, never taken from the message.
    pub fn apply(&mut self, source: &str, mut r: UsageReport, now: u64) -> bool {
        if r.holder != source || !plausible(&r, now) {
            return false;
        }
        r.amount = price(&r);
        let cutoff = now.saturating_sub(KEEP_HOURS * HOUR_SECS);
        if self.reports.len() >= MAX_REPORTS {
            self.reports.retain(|_, x| x.hour >= cutoff);
            if self.reports.len() >= MAX_REPORTS {
                return false;
            }
        }
        self.reports.insert((r.app_id.clone(), r.holder.clone(), r.hour), r);
        true
    }

    /// Reports for hours starting in `[from, to)`.
    pub fn between(&self, from: u64, to: u64) -> Vec<UsageReport> {
        let mut out: Vec<UsageReport> = self.reports.values().filter(|r| (from..to).contains(&r.hour)).cloned().collect();
        out.sort_by(|a, b| a.hour.cmp(&b.hour).then(a.app_id.cmp(&b.app_id)).then(a.holder.cmp(&b.holder)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: u64 = 1_800_000_000 / HOUR_SECS * HOUR_SECS;

    #[test]
    fn docker_stats_lines_parse() {
        let line = r#"{"BlockIO":"0B / 0B","CPUPerc":"150.25%","Container":"c","MemPerc":"1%","MemUsage":"512MiB / 2GiB","Name":"x","NetIO":"1.5kB / 2MB","PIDs":"3"}"#;
        let s = parse_stats(line).unwrap();
        assert!((s.cpus - 1.5025).abs() < 1e-9);
        assert_eq!(s.mem_bytes, 512 * 1024 * 1024);
        assert_eq!((s.rx_bytes, s.tx_bytes), (1500, 2_000_000));
        assert_eq!(parse_size("648B"), Some(648));
        assert_eq!(parse_size("1GiB"), Some(1 << 30));
        assert_eq!(parse_size("--"), None);
        assert!(parse_stats("{}").is_none());
    }

    #[test]
    fn an_hour_is_metered_and_priced() {
        let rates = Rates::default();
        let mut m = Meter::default();
        let s = Stats { cpus: 0.5, mem_bytes: 1 << 30, rx_bytes: 0, tx_bytes: 0 };
        m.sample("app", "own", "me", rates, s, 2 << 30, H); // baseline
        for i in 1..=59 {
            let s = Stats { tx_bytes: i * (1 << 30) / 60, ..s };
            m.sample("app", "own", "me", rates, s, 2 << 30, H + i * 60);
        }
        assert!(m.take_finished("me", rates, H + 3599).is_empty(), "the hour is not over");
        // The sample on the boundary closes the hour with its last minute.
        m.sample("app", "own", "me", rates, Stats { tx_bytes: 1 << 30, ..s }, 2 << 30, H + 3600);
        let r = m.take_finished("me", rates, H + 3600 + 1);
        assert_eq!(r.len(), 1);
        let r = &r[0];
        assert_eq!((r.hour, r.secs), (H, 3600));
        assert!((r.cpu_s - 1800.0).abs() < 1e-6);
        assert!((r.ram_gib_h - 1.0).abs() < 1e-9);
        assert!((r.disk_gib_h - 2.0).abs() < 1e-9);
        assert_eq!(r.tx_bytes, 1 << 30);
        // 0.5 CPU-h ($0.005) + 1 GiB-h RAM ($0.002) + 2 GiB-h disk ($0.004 x 2/720) + 1 GiB out ($0.01)
        let want = 5_000.0 + 2_000.0 + 4_000.0 * 2.0 / 720.0 + 10_000.0;
        assert_eq!(r.amount, want as Micros);
    }

    #[test]
    fn gaps_are_capped_restarts_count_new_traffic_and_hours_split() {
        let rates = Rates::default();
        let mut m = Meter::default();
        let s = Stats { cpus: 1.0, mem_bytes: 0, rx_bytes: 100, tx_bytes: 100 };
        m.sample("a", "o", "me", rates, s, 0, H + 3000);
        // 700 s later, across the boundary: at most MAX_SAMPLE_GAP_SECS
        // counted, all of it in the old hour, with the traffic since (the
        // counter fell 100 -> 40: a restart, so 40 new bytes).
        m.sample("a", "o", "me", rates, Stats { tx_bytes: 40, ..s }, 0, H + 3600 + 100);
        let first = m.take_finished("me", rates, H + 3600 + 100);
        assert_eq!(first.len(), 1);
        assert_eq!((first[0].hour, first[0].secs, first[0].tx_bytes), (H, MAX_SAMPLE_GAP_SECS, 40));
        m.sample("a", "o", "me", rates, Stats { tx_bytes: 50, ..s }, 0, H + 3600 + 160); // +10
        // Restarted again: its counter fell to 5, all of it new.
        m.sample("a", "o", "me", rates, Stats { tx_bytes: 5, ..s }, 0, H + 3600 + 220); // +5
        // A 20-minute gap counts only MAX_SAMPLE_GAP_SECS.
        m.sample("a", "o", "me", rates, Stats { tx_bytes: 5, ..s }, 0, H + 3600 + 1420);
        let r = m.take_finished("me", rates, H + 7200);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].hour, H + 3600);
        assert!((r[0].cpu_s - (60.0 + 60.0 + MAX_SAMPLE_GAP_SECS as f64)).abs() < 1e-9);
        assert_eq!(r[0].tx_bytes, 15);
    }

    #[test]
    fn book_keeps_only_what_the_holder_says_about_itself() {
        let mut b = Book::default();
        let rates = Rates::default();
        let mut r = UsageReport {
            app_id: "a".into(),
            owner: "o".into(),
            holder: "peerA".into(),
            hour: H,
            secs: 3600,
            cpu_s: 3600.0,
            ram_gib_h: 0.0,
            disk_gib_h: 0.0,
            rx_bytes: 0,
            tx_bytes: 0,
            rates,
            amount: 999_999_999,
        };
        let now = H + 3700;
        assert!(!b.apply("peerB", r.clone(), now), "someone else's report");
        assert!(b.apply("peerA", r.clone(), now));
        assert_eq!(b.between(H, H + 3600)[0].amount, 10_000, "amount recomputed");
        r.cpu_s = f64::NAN;
        assert!(!b.apply("peerA", r.clone(), now));
        r.cpu_s = 1.0;
        r.hour = H + 1;
        assert!(!b.apply("peerA", r, now), "not on an hour");
    }

    #[test]
    fn rates_from_config() {
        let r = Rates::from_config("0.02", "", "bad", "0.05");
        assert_eq!(r, Rates { cpu_hour: 20_000, ram_gib_hour: 2_000, disk_gib_month: 4_000, egress_gib: 50_000 });
    }
}
