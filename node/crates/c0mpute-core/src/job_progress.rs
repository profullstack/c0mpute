//! Live progress for long gateway jobs (songs), so a caller waiting minutes
//! on `POST /v1/music` can show what is happening instead of a timer.
//!
//! The caller names its request with an `x-request-id` header; the gateway
//! records each stage under that id (finding a worker, queued behind busy
//! workers, rendering on node X with its ETA, fetching the result, retrying)
//! and `GET /v1/progress/{id}` returns the latest. The job code reports
//! through a task-local, so the shared auction path needs no new arguments.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// How long a finished request's progress stays readable.
const KEEP_MS: u64 = 30 * 60 * 1000;
const MAX_ENTRIES: usize = 512;

#[derive(Clone, Debug, Serialize)]
pub struct JobProgress {
    /// starting | finding_worker | queued | rendering | fetching_result |
    /// retrying | done | failed
    pub stage: &'static str,
    /// Auction attempt (a worker that fails or vanishes means another).
    pub attempt: u32,
    /// The winning worker's peer id while it renders.
    pub worker: Option<String>,
    /// The c0mpute job id of the current attempt.
    pub job_id: Option<String>,
    /// The winning worker's own estimate, from its measured job times.
    pub eta_s: Option<u64>,
    /// Why the last attempt was retried, when it was.
    pub detail: Option<String>,
    pub started_at_ms: u64,
    pub stage_since_ms: u64,
    /// Longest this request waits in the queue for a worker.
    pub max_wait_s: Option<u64>,
}

impl JobProgress {
    pub fn finished(&self) -> bool {
        self.stage == "done" || self.stage == "failed"
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Default)]
pub struct ProgressBoard(Arc<Mutex<HashMap<String, JobProgress>>>);

impl ProgressBoard {
    pub fn start(&self, key: &str) {
        let now = now_ms();
        let Ok(mut map) = self.0.lock() else { return };
        // A request still queued or running stays readable however long it
        // waits (a queued job may wait hours); finished ones for KEEP_MS.
        map.retain(|_, p| !p.finished() || now.saturating_sub(p.stage_since_ms) < KEEP_MS);
        if map.len() >= MAX_ENTRIES
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, p)| (!p.finished(), p.started_at_ms))
                .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
        map.insert(
            key.to_string(),
            JobProgress {
                stage: "starting",
                attempt: 0,
                worker: None,
                job_id: None,
                eta_s: None,
                detail: None,
                started_at_ms: now,
                stage_since_ms: now,
                max_wait_s: None,
            },
        );
    }

    pub fn update(&self, key: &str, f: impl FnOnce(&mut JobProgress)) {
        if let Ok(mut map) = self.0.lock()
            && let Some(p) = map.get_mut(key)
        {
            f(p);
        }
    }

    /// The entry plus how long the request and its current stage have run.
    pub fn snapshot(&self, key: &str) -> Option<serde_json::Value> {
        let map = self.0.lock().ok()?;
        let p = map.get(key)?;
        let now = now_ms();
        let mut v = serde_json::to_value(p).ok()?;
        v["elapsed_s"] = (now.saturating_sub(p.started_at_ms) / 1000).into();
        v["stage_elapsed_s"] = (now.saturating_sub(p.stage_since_ms) / 1000).into();
        Some(v)
    }
}

/// A finished tracked request, kept so a caller that lost its connection (an
/// app restarting mid-song) can still collect it: `GET /v1/result/{id}`.
#[derive(Clone, Debug)]
pub enum Stored {
    Done {
        bytes: Arc<Vec<u8>>,
        content_type: &'static str,
        job_id: String,
        worker: String,
    },
    Failed {
        status: u16,
        message: String,
    },
}

/// How many finished results are held at once (songs are ~2-5 MB each).
const MAX_RESULTS: usize = 48;

#[derive(Clone, Default)]
pub struct ResultStore(Arc<Mutex<HashMap<String, (u64, Stored)>>>, Option<Arc<std::path::PathBuf>>);

/// On-disk form of a stored result's metadata (bytes live beside it).
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredMeta {
    at_ms: u64,
    ok: bool,
    content_type: String,
    job_id: String,
    worker: String,
    status: u16,
    message: String,
}

fn static_content_type(t: &str) -> &'static str {
    match t {
        "audio/flac" => "audio/flac",
        "audio/wav" => "audio/wav",
        "image/png" => "image/png",
        "video/mp4" => "video/mp4",
        _ => "audio/mpeg",
    }
}

impl ResultStore {
    /// A store that also keeps results under `dir`, so they outlive a restart.
    pub fn persistent(dir: std::path::PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        // Drop files older than KEEP_MS left by earlier runs.
        let now = now_ms();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let old = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .is_some_and(|d| now.saturating_sub(d.as_millis() as u64) > KEEP_MS);
                if old {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        Self(Arc::default(), Some(Arc::new(dir)))
    }

    fn write_disk(&self, key: &str, at_ms: u64, stored: &Stored) {
        let Some(dir) = self.1.as_deref() else { return };
        let meta = match stored {
            Stored::Done { bytes, content_type, job_id, worker } => {
                if std::fs::write(dir.join(format!("{key}.bin")), bytes.as_slice()).is_err() {
                    return;
                }
                StoredMeta { at_ms, ok: true, content_type: content_type.to_string(), job_id: job_id.clone(), worker: worker.clone(), status: 200, message: String::new() }
            }
            Stored::Failed { status, message } => StoredMeta {
                at_ms, ok: false, content_type: String::new(), job_id: String::new(), worker: String::new(), status: *status, message: message.clone(),
            },
        };
        if let Ok(json) = serde_json::to_vec(&meta) {
            let _ = std::fs::write(dir.join(format!("{key}.json")), json);
        }
    }

    fn read_disk(&self, key: &str) -> Option<(u64, Stored)> {
        let dir = self.1.as_deref()?;
        let meta: StoredMeta = serde_json::from_slice(&std::fs::read(dir.join(format!("{key}.json"))).ok()?).ok()?;
        let stored = if meta.ok {
            Stored::Done {
                bytes: Arc::new(std::fs::read(dir.join(format!("{key}.bin"))).ok()?),
                content_type: static_content_type(&meta.content_type),
                job_id: meta.job_id,
                worker: meta.worker,
            }
        } else {
            Stored::Failed { status: meta.status, message: meta.message }
        };
        Some((meta.at_ms, stored))
    }

    pub fn put(&self, key: &str, stored: Stored) {
        let now = now_ms();
        self.write_disk(key, now, &stored);
        let Ok(mut map) = self.0.lock() else { return };
        map.retain(|_, (at, _)| now.saturating_sub(*at) < KEEP_MS);
        while map.len() >= MAX_RESULTS {
            let Some(oldest) = map.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone()) else {
                break;
            };
            map.remove(&oldest);
        }
        map.insert(key.to_string(), (now, stored));
    }

    pub fn get(&self, key: &str) -> Option<Stored> {
        let now = now_ms();
        let hit = {
            let map = self.0.lock().ok()?;
            map.get(key).cloned()
        };
        // After a restart the memory is empty; the disk copy still answers.
        let (at, stored) = hit.or_else(|| self.read_disk(key))?;
        (now.saturating_sub(at) < KEEP_MS).then_some(stored)
    }
}

/// A tracked request that has not finished: enough to run it again after a
/// gateway restart, under the same request id.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingJob {
    pub workload: String,
    pub spec: serde_json::Value,
    pub required: Vec<String>,
    pub max_bytes: usize,
    pub started_at_ms: u64,
    /// The request's queue limit; older pending files predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wait_ms: Option<u64>,
}

/// How long a pending request without a recorded limit is resumable.
const DEFAULT_RESUME_MS: u64 = 2 * 60 * 60 * 1000;

impl PendingJob {
    /// Still worth resuming at `now`: within its queue limit (plus the keep
    /// window, for a job that was already running when the gateway stopped).
    pub fn resumable_at(&self, now: u64) -> bool {
        let limit = self.max_wait_ms.unwrap_or(DEFAULT_RESUME_MS).max(KEEP_MS) + KEEP_MS;
        now.saturating_sub(self.started_at_ms) < limit
    }
}

/// Unfinished tracked requests on disk (`<dir>/<id>.json`).
#[derive(Clone, Default)]
pub struct PendingStore(Option<Arc<std::path::PathBuf>>);

impl PendingStore {
    pub fn persistent(dir: std::path::PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self(Some(Arc::new(dir)))
    }

    pub fn save(&self, key: &str, job: &PendingJob) {
        let Some(dir) = self.0.as_deref() else { return };
        if let Ok(json) = serde_json::to_vec(job) {
            let _ = std::fs::write(dir.join(format!("{key}.json")), json);
        }
    }

    pub fn remove(&self, key: &str) {
        if let Some(dir) = self.0.as_deref() {
            let _ = std::fs::remove_file(dir.join(format!("{key}.json")));
        }
    }

    /// Unfinished requests younger than the keep window; older ones are dropped.
    pub fn take_resumable(&self) -> Vec<(String, PendingJob)> {
        let Some(dir) = self.0.as_deref() else { return Vec::new() };
        let now = now_ms();
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = e.path();
            let Some(key) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else { continue };
            let job = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<PendingJob>(&b).ok());
            match job {
                Some(job) if valid_key(&key) && job.resumable_at(now) => out.push((key, job)),
                _ => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        out
    }
}

pub fn now_ms_pub() -> u64 {
    now_ms()
}

/// A caller-chosen request id: short and URL-safe.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

tokio::task_local! {
    /// The board and request id of the job running on this task, if any.
    pub static CURRENT: (ProgressBoard, String);
    /// The queue limit of the request running on this task, and when it was
    /// first queued (unix ms; kept across a gateway restart).
    pub static QUEUE: QueueCtx;
}

/// See [`QUEUE`].
#[derive(Clone, Copy, Debug)]
pub struct QueueCtx {
    pub max_wait: std::time::Duration,
    pub queued_at_ms: u64,
}

/// The current task's queue context, if it runs inside one.
pub fn queue_ctx() -> Option<QueueCtx> {
    QUEUE.try_with(|q| *q).ok()
}

/// Change the current request's progress; a no-op outside a tracked job.
pub fn report(f: impl FnOnce(&mut JobProgress)) {
    let _ = CURRENT.try_with(|(board, key)| board.update(key, f));
}

/// Move the current request to `stage` (the stage clock restarts only on a change).
pub fn set_stage(stage: &'static str) {
    report(|p| {
        if p.stage != stage {
            p.stage = stage;
            p.stage_since_ms = now_ms();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stages_are_recorded_under_the_request_id_only_inside_its_scope() {
        let board = ProgressBoard::default();
        board.start("req-1");
        set_stage("rendering"); // outside any scope: ignored
        assert_eq!(board.snapshot("req-1").unwrap()["stage"], "starting");
        CURRENT
            .scope((board.clone(), "req-1".to_string()), async {
                set_stage("queued");
                report(|p| {
                    p.worker = Some("12D3KooW".into());
                    p.eta_s = Some(140);
                });
            })
            .await;
        let s = board.snapshot("req-1").unwrap();
        assert_eq!(s["stage"], "queued");
        assert_eq!(s["worker"], "12D3KooW");
        assert_eq!(s["eta_s"], 140);
        assert!(s["elapsed_s"].is_u64());
        assert!(board.snapshot("nope").is_none());
    }

    #[test]
    fn finished_results_are_kept_for_a_caller_that_reconnects() {
        let store = ResultStore::default();
        assert!(store.get("song-1").is_none());
        store.put(
            "song-1",
            Stored::Done {
                bytes: Arc::new(vec![1, 2, 3]),
                content_type: "audio/mpeg",
                job_id: "j".into(),
                worker: "w".into(),
            },
        );
        store.put("song-2", Stored::Failed { status: 503, message: "no worker".into() });
        assert!(matches!(store.get("song-1"), Some(Stored::Done { ref bytes, .. }) if bytes.as_slice() == [1, 2, 3]));
        assert!(matches!(store.get("song-2"), Some(Stored::Failed { status: 503, .. })));
        for i in 0..(MAX_RESULTS + 5) {
            store.put(&format!("k{i}"), Stored::Failed { status: 502, message: String::new() });
        }
        assert!(store.0.lock().unwrap().len() <= MAX_RESULTS, "bounded");
    }

    #[test]
    fn results_and_pending_jobs_survive_a_restart_on_disk() {
        let dir = std::env::temp_dir().join(format!("c0-jobs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let results = ResultStore::persistent(dir.join("results"));
            results.put(
                "song-9",
                Stored::Done { bytes: Arc::new(b"ID3song".to_vec()), content_type: "audio/mpeg", job_id: "j9".into(), worker: "w9".into() },
            );
            let pending = PendingStore::persistent(dir.join("jobs"));
            pending.save(
                "song-10",
                &PendingJob { workload: "music.txt2music".into(), spec: serde_json::json!({"prompt": "x"}), required: vec![], max_bytes: 1, started_at_ms: now_ms(), max_wait_ms: None },
            );
            pending.save(
                "stale",
                &PendingJob { workload: "music.txt2music".into(), spec: serde_json::json!({}), required: vec![], max_bytes: 1, started_at_ms: 1, max_wait_ms: None },
            );
        }
        // "Restart": fresh stores over the same directories.
        let results = ResultStore::persistent(dir.join("results"));
        assert!(matches!(results.get("song-9"), Some(Stored::Done { ref bytes, content_type: "audio/mpeg", ref worker, .. }) if bytes.as_slice() == b"ID3song" && worker == "w9"));
        let pending = PendingStore::persistent(dir.join("jobs"));
        let resumable = pending.take_resumable();
        assert_eq!(resumable.len(), 1, "the stale one is dropped");
        assert_eq!(resumable[0].0, "song-10");
        pending.remove("song-10");
        assert!(pending.take_resumable().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn request_ids_must_be_short_and_url_safe() {
        assert!(valid_key("17a1b21d-a95e-4345-a3fe-d14dfd6868e3"));
        assert!(!valid_key(""));
        assert!(!valid_key("../etc"));
        assert!(!valid_key(&"a".repeat(65)));
    }

    #[test]
    fn a_queued_job_stays_resumable_for_its_whole_wait() {
        let now = 10 * 60 * 60 * 1000;
        let job = |age_ms: u64, max_wait_ms: Option<u64>| PendingJob {
            workload: "diffusion.txt2img".into(),
            spec: serde_json::json!({}),
            required: vec![],
            max_bytes: 1,
            started_at_ms: now - age_ms,
            max_wait_ms,
        };
        let hour = 60 * 60 * 1000;
        // Queued 90 min with a 2 h limit: resumed (the old rule dropped it at 30 min).
        assert!(job(90 * 60 * 1000, Some(2 * hour)).resumable_at(now));
        assert!(!job(3 * hour, Some(2 * hour)).resumable_at(now));
        // Files from before the limit was recorded get the 2 h default.
        assert!(job(90 * 60 * 1000, None).resumable_at(now));
    }

    #[test]
    fn a_long_queued_request_keeps_its_progress_entry() {
        let board = ProgressBoard::default();
        board.start("q");
        board.update("q", |p| {
            p.stage = "queued";
            p.started_at_ms -= 2 * KEEP_MS;
            p.stage_since_ms -= 2 * KEEP_MS;
        });
        board.start("other"); // runs the retain
        assert!(board.snapshot("q").is_some(), "still queued, still visible");
        board.update("q", |p| p.stage = "done");
        board.start("third");
        assert!(board.snapshot("q").is_none(), "finished long ago, dropped");
    }

}
