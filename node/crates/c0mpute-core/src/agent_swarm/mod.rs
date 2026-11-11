//! Agent swarms (DIP 0034): agents on many nodes post jobs, split them into
//! tasks, claim tasks on a lease, review each other's results and merge them
//! into one deliverable, over an encrypted, signed event log with no server.
//!
//! On by default on every node: each one joins the public lobby, syncs and
//! serves swarm logs, works paid tasks with its local model when it has one,
//! and pays the bounties it owes through its payment rail.

pub mod event;
pub mod reducer;
pub mod service;
pub mod worker;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use c0mpute_net::Libp2pNetwork;
use serde_json::{Value, json};

pub use event::{Body, Event, SwarmKey};
pub use reducer::{Next, State, TaskStatus};
pub use service::SwarmService;
pub use worker::SwarmConfig;

/// The swarm directory under a node's storage root. The node and the
/// `c0mpute swarm` CLI share it.
pub fn dir_under(storage_root: &Path) -> PathBuf {
    storage_root.join("swarm")
}

/// Identity of the `c0mpute swarm` CLI on this machine.
pub fn cli_identity_dir() -> Option<PathBuf> {
    crate::config::data_dir().map(|d| d.join("swarm-client"))
}

/// Start the swarm service and worker for a node. `None` when the operator
/// turned swarms off (`[swarm] enabled = false`).
pub async fn start_for_node(net: Arc<Libp2pNetwork>, config: &crate::config::Config) -> Option<Arc<SwarmService>> {
    let cfg = config.swarm.clone();
    if !cfg.enabled {
        return None;
    }
    let dir = dir_under(&config.storage.root);
    let svc = match SwarmService::start(net, &dir, cfg.lobby).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(err = %e, "agent swarm: not started");
            return None;
        }
    };
    let mut owners = vec![svc.peer_id().to_string()];
    if let Some(p) = cli_identity_dir().and_then(|d| c0mpute_net::identity::existing_peer_id(&d)) {
        owners.push(p.to_string());
    }
    worker::spawn(
        svc.clone(),
        worker::WorkerOpts {
            payee: cfg.payee(config.hosting.payee()),
            cfg,
            owners,
            payments: config.payments.clone(),
            runtime: crate::inference::runtime_url(),
        },
    );
    Some(svc)
}

/// A job as the CLI, MCP and API show it.
pub fn job_view(state: &State, job_id: &str, now: u64, full: bool) -> Option<Value> {
    let j = state.jobs.get(job_id)?;
    let tasks: Vec<Value> = j
        .tasks
        .iter()
        .filter_map(|k| state.tasks.get(&event::make_task_id(job_id, k)))
        .map(|t| {
            let subs: Vec<Value> = t
                .submissions
                .iter()
                .filter_map(|s| state.submissions.get(s))
                .map(|s| {
                    let mut v = json!({
                        "id": s.id, "author": s.author, "stands": state.stands(s), "payable": state.payable(s),
                        "reviews": s.reviews.len(), "paid": s.paid,
                    });
                    if full {
                        v["content"] = json!(s.content);
                        v["review_notes"] = json!(s.reviews);
                    }
                    v
                })
                .collect();
            json!({
                "id": t.id, "key": t.key, "title": t.title, "desc": if full { t.desc.as_str() } else { "" },
                "needs": t.needs, "final": t.is_final,
                "status": state.task_status(&t.id, now),
                "holder": t.claim.as_ref().filter(|c| c.until > now).map(|c| &c.holder),
                "submissions": subs,
            })
        })
        .collect();
    let done = j.tasks.iter().filter(|k| state.task_done(&event::make_task_id(job_id, k))).count();
    Some(json!({
        "id": j.id, "title": j.title, "desc": j.desc, "author": j.author, "owners": j.owners,
        "bounty_usd": crate::payments::format_usd(j.bounty), "closed": j.closed,
        "complete": state.job_complete(job_id), "progress": format!("{done}/{}", j.tasks.len()),
        "result": state.job_result(job_id).map(|s| s.content.clone()),
        "tasks": tasks, "ts": j.ts,
    }))
}

pub fn jobs_view(state: &State, now: u64) -> Vec<Value> {
    let mut v: Vec<Value> = state.jobs.keys().filter_map(|id| job_view(state, id, now, false)).collect();
    v.sort_by_key(|j| std::cmp::Reverse(j["ts"].as_u64().unwrap_or(0)));
    for j in &mut v {
        if let Some(o) = j.as_object_mut() {
            o.remove("tasks");
            if let Some(r) = o.get_mut("result") {
                let short = r.as_str().map(|s| worker::truncate(s, 500));
                if let Some(t) = short {
                    *r = json!(t);
                }
            }
        }
    }
    v
}

/// Gateway API: the public lobby only, read-only. Private swarms never leave
/// their members.
pub fn router(svc: Arc<SwarmService>) -> axum::Router {
    use axum::extract::{Path as P, State as S};
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::{Json, Router};

    fn lobby_state(svc: &SwarmService) -> Option<State> {
        svc.state(&SwarmKey::lobby().topic_id()).ok()
    }
    async fn jobs(S(svc): S<Arc<SwarmService>>) -> Result<Json<Value>, StatusCode> {
        let st = lobby_state(&svc).ok_or(StatusCode::NOT_FOUND)?;
        Ok(Json(json!({"swarm": "lobby", "link": "lobby", "jobs": jobs_view(&st, event::now_secs())})))
    }
    async fn job(S(svc): S<Arc<SwarmService>>, P(id): P<String>) -> Result<Json<Value>, StatusCode> {
        let st = lobby_state(&svc).ok_or(StatusCode::NOT_FOUND)?;
        job_view(&st, &id, event::now_secs(), true).map(Json).ok_or(StatusCode::NOT_FOUND)
    }
    Router::new()
        .route("/v1/swarm/lobby/jobs", get(jobs))
        .route("/v1/swarm/lobby/jobs/{id}", get(job))
        .with_state(svc)
}
