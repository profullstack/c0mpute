//! Worker side of resuming and cancelling a job (DIP-0032).
//!
//! A buyer that restarts mid-job (a gateway updating while an image renders)
//! re-derives the job's keys from its job secret and the job id, then asks
//! the winner where the job stands over `c0mpute.job.control.v1`:
//!
//! - `running`: keep waiting for the receipt (no re-auction, no second run);
//! - `done`: here is the receipt (the result is still in the worker's store,
//!   sealed to the same key);
//! - `cancelled` / `unknown`: the buyer re-auctions.
//!
//! The buyer can also `cancel` a job it abandoned: the worker aborts it, so a
//! render nobody will collect stops using the worker. Only the job's buyer
//! (its peer id, authenticated by the noise handshake) may query or cancel.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use c0mpute_net::{Libp2pNetwork, PeerId, RpcHandler};
use serde::{Deserialize, Serialize};

pub const CONTROL_SERVICE: &str = "c0mpute.job.control.v1";
/// How long a finished job's receipt stays available to its buyer.
const KEEP_DONE: Duration = Duration::from_secs(2 * 60 * 60);
const MAX_ENTRIES: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Status,
    Cancel,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub op: Op,
    pub job_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Done,
    Cancelled,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub state: State,
    /// The job's receipt (JSON), when it is done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<serde_json::Value>,
}

enum Entry {
    Running { abort: Option<futures::future::AbortHandle> },
    Done { receipt: serde_json::Value },
    Cancelled,
}

struct Job {
    buyer: String,
    entry: Entry,
    at: Instant,
}

fn jobs() -> &'static Mutex<HashMap<String, Job>> {
    static J: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new();
    J.get_or_init(Default::default)
}

fn prune(m: &mut HashMap<String, Job>) {
    let now = Instant::now();
    m.retain(|_, j| matches!(j.entry, Entry::Running { .. }) || now.duration_since(j.at) < KEEP_DONE);
    if m.len() > MAX_ENTRIES {
        let mut by_age: Vec<(String, Instant)> = m.iter().map(|(k, j)| (k.clone(), j.at)).collect();
        by_age.sort_by_key(|(_, at)| *at);
        for (k, _) in by_age.into_iter().take(m.len() - MAX_ENTRIES) {
            m.remove(&k);
        }
    }
}

/// A won job is about to run for `buyer`.
pub fn running(job_id: &str, buyer: &str) {
    if let Ok(mut m) = jobs().lock() {
        prune(&mut m);
        m.insert(job_id.to_string(), Job { buyer: buyer.to_string(), entry: Entry::Running { abort: None }, at: Instant::now() });
    }
}

/// Run a job's work so that a `cancel` from its buyer stops it: the work is
/// dropped at its next await point (child processes die with it, runners
/// spawn them with `kill_on_drop`) and this returns an error.
pub async fn cancellable<F, T>(job_id: &str, fut: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    let (fut, handle) = futures::future::abortable(fut);
    if let Ok(mut m) = jobs().lock()
        && let Some(j) = m.get_mut(job_id)
    {
        match &mut j.entry {
            Entry::Running { abort } => *abort = Some(handle),
            // Cancelled before it started.
            Entry::Cancelled => handle.abort(),
            Entry::Done { .. } => {}
        }
    }
    match fut.await {
        Ok(r) => r,
        Err(futures::future::Aborted) => anyhow::bail!("cancelled by the buyer"),
    }
}

/// The job finished: keep its receipt for a buyer that missed it.
pub fn done(job_id: &str, receipt: &impl Serialize) {
    if let (Ok(mut m), Ok(v)) = (jobs().lock(), serde_json::to_value(receipt))
        && let Some(j) = m.get_mut(job_id)
        && !matches!(j.entry, Entry::Cancelled)
    {
        j.entry = Entry::Done { receipt: v };
        j.at = Instant::now();
    }
}

/// Drop a job this process will not run after all (its twin won it).
pub fn forget(job_id: &str) {
    if let Ok(mut m) = jobs().lock() {
        m.remove(job_id);
    }
}

/// Whether the buyer cancelled this job (its receipt is then not sent).
pub fn cancelled(job_id: &str) -> bool {
    jobs().lock().ok().is_some_and(|m| m.get(job_id).is_some_and(|j| matches!(j.entry, Entry::Cancelled)))
}

/// Answer a request from `peer`.
pub fn handle(peer: &str, req: &Request) -> Response {
    let Ok(mut m) = jobs().lock() else {
        return Response { state: State::Unknown, receipt: None };
    };
    let Some(j) = m.get_mut(&req.job_id).filter(|j| j.buyer == peer) else {
        // Same answer for "no such job" and "not your job".
        return Response { state: State::Unknown, receipt: None };
    };
    match (&req.op, &mut j.entry) {
        (Op::Status, Entry::Running { .. }) => Response { state: State::Running, receipt: None },
        (Op::Status, Entry::Done { receipt }) => Response { state: State::Done, receipt: Some(receipt.clone()) },
        (_, Entry::Cancelled) => Response { state: State::Cancelled, receipt: None },
        (Op::Cancel, Entry::Running { abort }) => {
            if let Some(a) = abort.take() {
                a.abort();
            }
            j.entry = Entry::Cancelled;
            j.at = Instant::now();
            Response { state: State::Cancelled, receipt: None }
        }
        // Too late to cancel: report what happened.
        (Op::Cancel, Entry::Done { receipt }) => Response { state: State::Done, receipt: Some(receipt.clone()) },
    }
}

struct Handler;

#[async_trait::async_trait]
impl RpcHandler for Handler {
    async fn handle(&self, peer: PeerId, body: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
        let req: Request = serde_json::from_slice(&body).map_err(|_| "bad request".to_string())?;
        if matches!(req.op, Op::Cancel) {
            tracing::info!(job_id = %req.job_id, "job control: cancel requested by the buyer");
        }
        serde_json::to_vec(&handle(&peer.to_base58(), &req)).map_err(|e| e.to_string())
    }
}

/// Serve [`CONTROL_SERVICE`] on this node (idempotent).
pub fn serve(net: &Libp2pNetwork) {
    net.register_rpc(CONTROL_SERVICE, Arc::new(Handler));
}

/// Buyer side: ask `worker` about `job_id`.
pub async fn ask(net: &Libp2pNetwork, worker: PeerId, op: Op, job_id: &str) -> anyhow::Result<Response> {
    let body = serde_json::to_vec(&Request { op, job_id: job_id.to_string() })?;
    let bytes = net.rpc(worker, CONTROL_SERVICE, body).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(op: Op, id: &str) -> Request {
        Request { op, job_id: id.into() }
    }

    #[tokio::test]
    async fn only_the_buyer_sees_or_cancels_and_cancel_aborts() {
        running("j-ctl-1", "buyer");
        let h = tokio::spawn(cancellable("j-ctl-1", async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        }));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(handle("someone", &req(Op::Status, "j-ctl-1")).state, State::Unknown);
        assert_eq!(handle("someone", &req(Op::Cancel, "j-ctl-1")).state, State::Unknown);
        assert_eq!(handle("buyer", &req(Op::Status, "j-ctl-1")).state, State::Running);
        assert_eq!(handle("buyer", &req(Op::Cancel, "j-ctl-1")).state, State::Cancelled);
        let r = tokio::time::timeout(Duration::from_secs(5), h).await.expect("aborted promptly").unwrap();
        assert!(r.is_err(), "the job's work was aborted");
        assert!(cancelled("j-ctl-1"));
        // A receipt after a cancel is not recorded as done.
        done("j-ctl-1", &serde_json::json!({"status": "completed"}));
        assert_eq!(handle("buyer", &req(Op::Status, "j-ctl-1")).state, State::Cancelled);
    }

    #[test]
    fn a_finished_job_hands_its_receipt_to_its_buyer() {
        running("j-ctl-2", "buyer");
        done("j-ctl-2", &serde_json::json!({"job_id": "j-ctl-2", "status": "completed"}));
        let r = handle("buyer", &req(Op::Status, "j-ctl-2"));
        assert_eq!(r.state, State::Done);
        assert_eq!(r.receipt.unwrap()["job_id"], "j-ctl-2");
        assert_eq!(handle("buyer", &req(Op::Status, "nope")).state, State::Unknown);
    }
}
