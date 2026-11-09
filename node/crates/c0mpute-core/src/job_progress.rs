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
        map.retain(|_, p| now.saturating_sub(p.started_at_ms) < KEEP_MS);
        if map.len() >= MAX_ENTRIES
            && let Some(oldest) = map.iter().min_by_key(|(_, p)| p.started_at_ms).map(|(k, _)| k.clone())
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

/// A caller-chosen request id: short and URL-safe.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

tokio::task_local! {
    /// The board and request id of the job running on this task, if any.
    pub static CURRENT: (ProgressBoard, String);
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
    fn request_ids_must_be_short_and_url_safe() {
        assert!(valid_key("17a1b21d-a95e-4345-a3fe-d14dfd6868e3"));
        assert!(!valid_key(""));
        assert!(!valid_key("../etc"));
        assert!(!valid_key(&"a".repeat(65)));
    }
}
