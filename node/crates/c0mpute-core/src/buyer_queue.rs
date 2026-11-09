//! The gateway's buyer-side queue: a job nobody bids on waits for a worker
//! instead of failing.
//!
//! A request whose auction gets no bid stays queued and is re-offered with
//! backoff (every [`QueueConfig::min_pause`] .. [`QueueConfig::max_pause`],
//! sooner when a node advertising the job's tags shows up) until it is served
//! or its maximum wait runs out (2 h by default). Only then does the caller
//! get `no worker for <workload>/<model> within 2h`.
//!
//! The limits come from the gateway's environment, per workload, and per
//! request (`x-max-wait` header or `max_wait` query parameter, seconds):
//!
//! | variable | default |
//! |---|---|
//! | `C0MPUTE_QUEUE_MAX_WAIT_SECS` | 7200 |
//! | `C0MPUTE_QUEUE_MAX_WAIT_<WORKLOAD>_SECS` (e.g. `..._DIFFUSION_TXT2IMG_SECS`) | the above |
//! | `C0MPUTE_QUEUE_MIN_PAUSE_SECS` / `C0MPUTE_QUEUE_MAX_PAUSE_SECS` | 5 / 30 |
//! | `C0MPUTE_QUEUE_MAX_JOBS` (per workload) | 200 |
//! | `C0MPUTE_SYNC_WAIT_SECS` (answer 202 after this) | 300 |
//!
//! Tracked requests (images, songs, clips) are on disk while queued, so a
//! restart or an auto-update resumes them where they were. [`QueueBoard`]
//! keeps them first-in first-out per workload: one workload's queue never
//! holds up another's, and each workload's queue is capped.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default longest wait for a worker.
pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(2 * 60 * 60);
/// Requests may ask for at most this.
pub const MAX_MAX_WAIT: Duration = Duration::from_secs(24 * 60 * 60);
/// Default answer-synchronously window before a `202`.
pub const DEFAULT_SYNC_WAIT: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
pub struct QueueConfig {
    pub default_max_wait: Duration,
    pub per_workload: HashMap<String, Duration>,
    pub min_pause: Duration,
    pub max_pause: Duration,
    pub max_jobs: usize,
    pub sync_wait: Duration,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            default_max_wait: DEFAULT_MAX_WAIT,
            per_workload: HashMap::new(),
            min_pause: Duration::from_secs(5),
            max_pause: Duration::from_secs(30),
            max_jobs: 200,
            sync_wait: DEFAULT_SYNC_WAIT,
        }
    }
}

/// `diffusion.txt2img` -> `DIFFUSION_TXT2IMG`.
pub fn env_key(workload: &str) -> String {
    workload
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect()
}

const KNOWN_WORKLOADS: &[&str] = &[
    "tts.synthesize",
    "stt.transcribe",
    "diffusion.txt2img",
    "music.txt2music",
    "video.txt2vid",
    "video.img2vid",
    "ocr.extract",
    "embed.text",
];

impl QueueConfig {
    /// From `C0MPUTE_QUEUE_*` / `C0MPUTE_SYNC_WAIT_SECS` (see the module docs).
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let secs = |k: &str| get(k).and_then(|v| v.trim().parse::<u64>().ok()).map(Duration::from_secs);
        let d = Self::default();
        let mut per_workload = HashMap::new();
        for w in KNOWN_WORKLOADS {
            if let Some(v) = secs(&format!("C0MPUTE_QUEUE_MAX_WAIT_{}_SECS", env_key(w))) {
                per_workload.insert(w.to_string(), v.min(MAX_MAX_WAIT));
            }
        }
        let min_pause = secs("C0MPUTE_QUEUE_MIN_PAUSE_SECS").unwrap_or(d.min_pause).max(Duration::from_secs(1));
        Self {
            default_max_wait: secs("C0MPUTE_QUEUE_MAX_WAIT_SECS").unwrap_or(d.default_max_wait).min(MAX_MAX_WAIT),
            per_workload,
            min_pause,
            max_pause: secs("C0MPUTE_QUEUE_MAX_PAUSE_SECS").unwrap_or(d.max_pause).max(min_pause),
            max_jobs: get("C0MPUTE_QUEUE_MAX_JOBS")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(d.max_jobs)
                .max(1),
            sync_wait: secs("C0MPUTE_SYNC_WAIT_SECS").unwrap_or(d.sync_wait),
        }
    }

    /// The limit for `workload`, or the request's own (capped at [`MAX_MAX_WAIT`]).
    pub fn max_wait_for(&self, workload: &str, requested: Option<Duration>) -> Duration {
        requested
            .map(|r| r.min(MAX_MAX_WAIT))
            .or_else(|| self.per_workload.get(workload).copied())
            .unwrap_or(self.default_max_wait)
    }

    /// Pause before the `round`-th re-offer (0-based): min, doubling, capped
    /// at max, never past what is left of the wait.
    pub fn backoff(&self, round: u32, left: Duration) -> Duration {
        let grown = self.min_pause.saturating_mul(1u32 << round.min(10));
        grown.min(self.max_pause).min(left)
    }
}

/// `5s`, `12 min`, `2h`, `1h30m`.
pub fn human(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=119 => format!("{s}s"),
        120..=3599 => format!("{} min", s / 60),
        _ if s % 3600 == 0 => format!("{}h", s / 3600),
        _ => format!("{}h{}m", s / 3600, (s % 3600) / 60),
    }
}

/// The model a job asks for: the spec's `model`, else a `...:model:<id>` tag.
pub fn model_of(spec: &serde_json::Value, required: &[String]) -> Option<String> {
    spec.get("model")
        .and_then(|m| m.as_str())
        .map(str::to_string)
        .or_else(|| required.iter().find_map(|t| t.split(":model:").nth(1).map(str::to_string)))
}

/// The error a caller gets when the wait runs out.
pub fn expired_message(workload: &str, model: Option<&str>, waited: Duration) -> String {
    match model {
        Some(m) => format!("no worker for {workload}/{m} within {}", human(waited)),
        None => format!("no worker for {workload} within {}", human(waited)),
    }
}

/// A request's own limit: `x-max-wait` header or `max_wait` query value, in
/// seconds.
pub fn requested_max_wait(header: Option<&str>, query: Option<&str>) -> Option<Duration> {
    header
        .or(query)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// How long to hold the HTTP request before answering `202`: RFC 7240
/// `Prefer: respond-async` (0) or `Prefer: wait=N`, else the gateway default.
pub fn sync_wait_from_prefer(prefer: Option<&str>, default: Duration) -> Duration {
    let Some(p) = prefer else { return default };
    let mut wait = default;
    for part in p.split([',', ';']) {
        let part = part.trim().to_ascii_lowercase();
        if part == "respond-async" {
            return Duration::ZERO;
        }
        if let Some(n) = part.strip_prefix("wait=").and_then(|n| n.trim().parse::<u64>().ok()) {
            wait = Duration::from_secs(n);
        }
    }
    wait
}

/// Queued and running tracked requests, first-in first-out per workload.
#[derive(Clone, Default)]
pub struct QueueBoard(Arc<Mutex<HashMap<String, Vec<(String, u64)>>>>);

impl QueueBoard {
    /// Add `key` to `workload`'s queue (queued at `at_ms`), unless that queue
    /// already holds `max` requests. Re-entering an existing key is a no-op.
    pub fn enter(&self, workload: &str, key: &str, at_ms: u64, max: usize) -> bool {
        let Ok(mut map) = self.0.lock() else { return true };
        let q = map.entry(workload.to_string()).or_default();
        if q.iter().any(|(k, _)| k == key) {
            return true;
        }
        if q.len() >= max {
            return false;
        }
        q.push((key.to_string(), at_ms));
        q.sort_by_key(|(_, at)| *at);
        true
    }

    pub fn leave(&self, workload: &str, key: &str) {
        if let Ok(mut map) = self.0.lock()
            && let Some(q) = map.get_mut(workload)
        {
            q.retain(|(k, _)| k != key);
        }
    }

    /// `(workload, 1-based position, queue length)` of `key`.
    pub fn position(&self, key: &str) -> Option<(String, usize, usize)> {
        let map = self.0.lock().ok()?;
        map.iter().find_map(|(w, q)| {
            q.iter().position(|(k, _)| k == key).map(|i| (w.clone(), i + 1, q.len()))
        })
    }

    pub fn len(&self, workload: &str) -> usize {
        self.0.lock().ok().and_then(|m| m.get(workload).map(Vec::len)).unwrap_or(0)
    }
}

/// Whose turn it is to auction, per queue (workload + required tags).
///
/// Every queued request used to run its own auction loop with its own
/// backoff, so when a worker freed up whichever request happened to be
/// re-offering won it: the newest requests (shortest backoff) jumped ahead
/// and the oldest could wait the whole two hours behind them. Now only the
/// oldest waiting request of a queue auctions; it keeps the turn through its
/// no-bid rounds and passes it on as soon as a worker accepted it (or it
/// gives up), so workers are handed out first-in first-out.
#[derive(Clone, Default)]
pub struct AuctionTurns {
    inner: Arc<Mutex<HashMap<String, QueueTurn>>>,
    changed: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct QueueTurn {
    /// Auctioning now; keeps the turn until it lets go.
    holder: Option<(u64, String)>,
    /// Waiting for the turn, oldest first.
    waiting: std::collections::BTreeSet<(u64, String)>,
}

/// Holding the turn of one queue; dropping it hands the turn on.
pub struct AuctionTurn {
    turns: AuctionTurns,
    queue: String,
    entry: (u64, String),
}

impl AuctionTurns {
    /// The queue a request waits in: its workload and the tags it needs.
    pub fn queue_of(workload: &str, required: &[String]) -> String {
        let mut tags: Vec<&str> = required.iter().map(String::as_str).collect();
        tags.sort_unstable();
        format!("{workload}|{}", tags.join(","))
    }

    /// Wait until `key` (queued at `queued_at_ms`) is the oldest request
    /// waiting to auction in `queue`.
    pub async fn acquire(&self, queue: &str, key: &str, queued_at_ms: u64) -> AuctionTurn {
        let entry = (queued_at_ms, key.to_string());
        if let Ok(mut map) = self.inner.lock() {
            map.entry(queue.to_string()).or_default().waiting.insert(entry.clone());
        }
        // Built before the guard so a drop between the check and the wait
        // still wakes us.
        let turn = AuctionTurn { turns: self.clone(), queue: queue.to_string(), entry };
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.try_take(queue, &turn.entry) {
                return turn;
            }
            notified.await;
        }
    }

    /// Take the turn if nobody holds it and `entry` is the oldest waiting.
    fn try_take(&self, queue: &str, entry: &(u64, String)) -> bool {
        let Ok(mut map) = self.inner.lock() else { return true };
        let Some(q) = map.get_mut(queue) else { return true };
        if q.holder.is_some() || q.waiting.first() != Some(entry) {
            return false;
        }
        q.waiting.remove(entry);
        q.holder = Some(entry.clone());
        true
    }

    /// Requests waiting for (or holding) the turn of `queue`.
    pub fn waiting(&self, queue: &str) -> usize {
        self.inner
            .lock()
            .ok()
            .and_then(|m| m.get(queue).map(|q| q.waiting.len() + usize::from(q.holder.is_some())))
            .unwrap_or(0)
    }
}

impl Drop for AuctionTurn {
    fn drop(&mut self) {
        if let Ok(mut map) = self.turns.inner.lock()
            && let Some(q) = map.get_mut(&self.queue)
        {
            // Holding the turn, or still waiting (gave up or was dropped).
            if q.holder.as_ref() == Some(&self.entry) {
                q.holder = None;
            } else {
                q.waiting.remove(&self.entry);
            }
            if q.holder.is_none() && q.waiting.is_empty() {
                map.remove(&self.queue);
            }
        }
        self.turns.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auction_turns_go_oldest_first_whatever_the_arrival_order() {
        let turns = AuctionTurns::default();
        let q = AuctionTurns::queue_of("diffusion.txt2img", &["b".into(), "a".into()]);
        assert_eq!(q, "diffusion.txt2img|a,b");
        // The newest request arrives first and takes the free turn.
        let newest = turns.acquire(&q, "new", 300).await;
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        // Then a middle and the oldest request queue behind it.
        for (key, at) in [("mid", 200u64), ("old", 100u64)] {
            let (turns, q, order) = (turns.clone(), q.clone(), order.clone());
            tasks.push(tokio::spawn(async move {
                let _t = turns.acquire(&q, key, at).await;
                order.lock().unwrap().push(key);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(turns.waiting(&q), 3);
        drop(newest);
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec!["old", "mid"]);
        assert_eq!(turns.waiting(&q), 0);
        // Another queue is never held up by this one.
        let other = AuctionTurns::queue_of("music.txt2music", &[]);
        let _a = turns.acquire(&q, "x", 1).await;
        let _b = tokio::time::timeout(Duration::from_millis(50), turns.acquire(&other, "y", 2))
            .await
            .expect("other queue has its own turn");
    }

    #[test]
    fn config_reads_env_per_workload_and_caps() {
        let env: HashMap<&str, &str> = [
            ("C0MPUTE_QUEUE_MAX_WAIT_SECS", "3600"),
            ("C0MPUTE_QUEUE_MAX_WAIT_VIDEO_TXT2VID_SECS", "999999"),
            ("C0MPUTE_QUEUE_MIN_PAUSE_SECS", "2"),
            ("C0MPUTE_QUEUE_MAX_PAUSE_SECS", "1"),
        ]
        .into_iter()
        .collect();
        let c = QueueConfig::from_lookup(|k| env.get(k).map(|v| v.to_string()));
        assert_eq!(c.max_wait_for("diffusion.txt2img", None), Duration::from_secs(3600));
        assert_eq!(c.max_wait_for("video.txt2vid", None), MAX_MAX_WAIT);
        assert_eq!(c.max_wait_for("diffusion.txt2img", Some(Duration::from_secs(60))), Duration::from_secs(60));
        assert_eq!(c.max_pause, Duration::from_secs(2), "max never under min");
        let d = QueueConfig::from_lookup(|_| None);
        assert_eq!(d.max_wait_for("music.txt2music", None), DEFAULT_MAX_WAIT);
    }

    #[test]
    fn backoff_goes_5_10_20_30() {
        let c = QueueConfig::default();
        let lots = Duration::from_secs(3600);
        let p: Vec<u64> = (0..6).map(|r| c.backoff(r, lots).as_secs()).collect();
        assert_eq!(p, vec![5, 10, 20, 30, 30, 30]);
        assert_eq!(c.backoff(9, Duration::from_secs(3)), Duration::from_secs(3));
    }

    #[test]
    fn expiry_message_names_workload_model_and_wait() {
        let spec = serde_json::json!({"model": "z-image-turbo"});
        let m = model_of(&spec, &[]);
        assert_eq!(
            expired_message("diffusion.txt2img", m.as_deref(), DEFAULT_MAX_WAIT),
            "no worker for diffusion.txt2img/z-image-turbo within 2h"
        );
        let tag = vec!["c0mpute:music:model:ace-step-1.5".to_string()];
        assert_eq!(model_of(&serde_json::json!({}), &tag).as_deref(), Some("ace-step-1.5"));
        assert_eq!(human(Duration::from_secs(12 * 60)), "12 min");
        assert_eq!(human(Duration::from_secs(5400)), "1h30m");
    }

    #[test]
    fn prefer_header_picks_the_sync_window() {
        let d = Duration::from_secs(300);
        assert_eq!(sync_wait_from_prefer(None, d), d);
        assert_eq!(sync_wait_from_prefer(Some("respond-async"), d), Duration::ZERO);
        assert_eq!(sync_wait_from_prefer(Some("wait=20"), d), Duration::from_secs(20));
        assert_eq!(sync_wait_from_prefer(Some("handling=lenient, respond-async"), d), Duration::ZERO);
        assert_eq!(requested_max_wait(Some("90"), None), Some(Duration::from_secs(90)));
        assert_eq!(requested_max_wait(None, Some("x")), None);
    }

    #[test]
    fn board_is_fifo_per_workload_and_capped() {
        let b = QueueBoard::default();
        assert!(b.enter("img", "b", 20, 2));
        assert!(b.enter("img", "a", 10, 2));
        assert!(b.enter("img", "a", 10, 2), "re-entering is fine");
        assert!(!b.enter("img", "c", 30, 2), "queue full");
        assert!(b.enter("song", "s", 5, 2), "another workload is not blocked");
        assert_eq!(b.position("a"), Some(("img".into(), 1, 2)));
        assert_eq!(b.position("b"), Some(("img".into(), 2, 2)));
        b.leave("img", "a");
        assert_eq!(b.position("b"), Some(("img".into(), 1, 1)));
        assert_eq!(b.len("song"), 1);
    }
}
