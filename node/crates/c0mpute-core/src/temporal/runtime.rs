//! Client for `infernet model serve --stdio`: Infernet's temporal runtime
//! contract (NDJSON, protocol 1) on a child process. c0mpute never links
//! Python; the child is Infernet's, which runs its own pinned runtime.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

/// The contract's error object (`code`, `message`, `retriable`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RuntimeError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retriable: bool,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RuntimeError {}

impl RuntimeError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        let retriable = matches!(code, "E_CAPACITY" | "E_TIMEOUT" | "E_RUNTIME");
        Self { code: code.into(), message: message.into(), retriable }
    }
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

pub struct RuntimeProc {
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    seq: AtomicU64,
    child: Mutex<Child>,
}

impl RuntimeProc {
    /// Spawn `<infernet> model serve --stdio [extra]`.
    pub async fn spawn(infernet: &Path, extra: &[String]) -> Result<Arc<Self>> {
        let mut child = Command::new(infernet)
            .args(["model", "serve", "--stdio"])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {} model serve --stdio", infernet.display()))?;
        let stdin = child.stdin.take().context("runtime stdin")?;
        let stdout = child.stdout.take().context("runtime stdout")?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let p2 = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                if msg.get("event").is_some() {
                    continue;
                }
                let Some(id) = msg.get("id").and_then(|v| v.as_str()).map(str::to_string) else { continue };
                if let Some(tx) = p2.lock().await.remove(&id) {
                    let _ = tx.send(msg);
                }
            }
            // Runtime gone: fail everything still waiting.
            for (_, tx) in p2.lock().await.drain() {
                let _ = tx.send(json!({"ok": false, "error": {"code": "E_RUNTIME", "message": "infernet runtime exited", "retriable": true}}));
            }
        });
        Ok(Arc::new(Self { stdin: Mutex::new(stdin), pending, seq: AtomicU64::new(0), child: Mutex::new(child) }))
    }

    pub async fn request(&self, op: &str, params: Value, timeout: Duration) -> std::result::Result<Value, RuntimeError> {
        let id = format!("c{}", self.seq.fetch_add(1, Ordering::Relaxed) + 1);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        let mut msg = json!({"v": 1, "id": id, "op": op});
        if let (Some(m), Value::Object(p)) = (msg.as_object_mut(), params) {
            for (k, v) in p {
                m.insert(k, v);
            }
        }
        let line = format!("{msg}\n");
        {
            let mut w = self.stdin.lock().await;
            if w.write_all(line.as_bytes()).await.is_err() || w.flush().await.is_err() {
                self.pending.lock().await.remove(&id);
                return Err(RuntimeError::new("E_RUNTIME", "infernet runtime is not running"));
            }
        }
        let reply = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => return Err(RuntimeError::new("E_RUNTIME", "infernet runtime dropped the request")),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(RuntimeError::new("E_TIMEOUT", format!("{op} timed out")));
            }
        };
        if reply.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            Ok(reply.get("result").cloned().unwrap_or(Value::Null))
        } else {
            Err(reply
                .get("error")
                .and_then(|e| serde_json::from_value(e.clone()).ok())
                .unwrap_or_else(|| RuntimeError::new("E_RUNTIME", "malformed runtime reply")))
        }
    }

    pub async fn shutdown(&self) {
        let _ = self.request("shutdown", json!({}), Duration::from_secs(5)).await;
        let _ = self.child.lock().await.kill().await;
    }
}
