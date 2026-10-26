//! Buyer-side transfer server for remote training: serves the dataset
//! bundle to the winning worker and receives the trained artifacts.
//!
//! Every URL carries a random 128-bit token; only three artifact names are
//! accepted. Put it behind https (`--public-url https://...`) when the
//! worker is outside your LAN: the token gates access, it does not encrypt.
//! The buyer verifies every artifact against the receipt's model hash.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use rand::RngCore;
use tokio::sync::Mutex;

pub const ARTIFACTS: [&str; 3] = ["manifest.json", "model.safetensors", "eval.json"];
const MAX_UPLOAD: usize = 1 << 30;

#[derive(Clone)]
struct St {
    token: String,
    dataset: Arc<Vec<u8>>,
    uploads: Arc<Mutex<HashMap<(String, String), Vec<u8>>>>,
}

pub struct TransferServer {
    pub base_url: String,
    pub local_addr: SocketAddr,
    token: String,
    uploads: Arc<Mutex<HashMap<(String, String), Vec<u8>>>>,
}

fn ok_slot(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

async fn get_dataset(State(st): State<St>, Path(token): Path<String>) -> Result<Vec<u8>, StatusCode> {
    if token != st.token {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(st.dataset.as_ref().clone())
}

async fn put_artifact(State(st): State<St>, Path((token, slot, file)): Path<(String, String, String)>, body: Bytes) -> StatusCode {
    if token != st.token || !ok_slot(&slot) || !ARTIFACTS.contains(&file.as_str()) {
        return StatusCode::NOT_FOUND;
    }
    st.uploads.lock().await.insert((slot, file), body.to_vec());
    StatusCode::CREATED
}

impl TransferServer {
    /// Bind `bind` and serve `dataset`. `public_base` is how the worker
    /// reaches this server (defaults to `http://<bound addr>`).
    pub async fn start(bind: SocketAddr, public_base: Option<String>, dataset: Vec<u8>) -> Result<Self> {
        let mut t = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut t);
        let token = hex::encode(t);
        let uploads = Arc::new(Mutex::new(HashMap::new()));
        let st = St { token: token.clone(), dataset: Arc::new(dataset), uploads: uploads.clone() };
        let app = Router::new()
            .route("/t/{token}/dataset.json", get(get_dataset))
            .route("/t/{token}/a/{slot}/{file}", put(put_artifact))
            .layer(DefaultBodyLimit::max(MAX_UPLOAD))
            .with_state(st);
        let listener = tokio::net::TcpListener::bind(bind).await?;
        let local_addr = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let base_url = public_base.unwrap_or_else(|| format!("http://{local_addr}")).trim_end_matches('/').to_string();
        Ok(Self { base_url, local_addr, token, uploads })
    }

    pub fn dataset_url(&self) -> String {
        format!("{}/t/{}/dataset.json", self.base_url, self.token)
    }

    pub fn upload_url(&self, slot: &str) -> String {
        format!("{}/t/{}/a/{}", self.base_url, self.token, slot)
    }

    /// Artifacts uploaded for `slot`, by file name.
    pub async fn take(&self, slot: &str) -> HashMap<String, Vec<u8>> {
        let mut u = self.uploads.lock().await;
        let keys: Vec<_> = u.keys().filter(|(s, _)| s == slot).cloned().collect();
        keys.into_iter().filter_map(|k| u.remove(&k).map(|v| (k.1, v))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn token_gates_dataset_and_uploads() {
        let s = TransferServer::start("127.0.0.1:0".parse().unwrap(), None, b"{\"x\":1}".to_vec()).await.unwrap();
        let c = reqwest::Client::new();
        assert_eq!(c.get(s.dataset_url()).send().await.unwrap().bytes().await.unwrap().as_ref(), b"{\"x\":1}");
        let wrong = s.dataset_url().replace(&s.token, &"0".repeat(32));
        assert_eq!(c.get(wrong).send().await.unwrap().status(), 404);
        let up = format!("{}/model.safetensors", s.upload_url("cfc"));
        assert_eq!(c.put(&up).body("w").send().await.unwrap().status(), 201);
        let bad = format!("{}/evil.sh", s.upload_url("cfc"));
        assert_eq!(c.put(&bad).body("x").send().await.unwrap().status(), 404);
        let got = s.take("cfc").await;
        assert_eq!(got.get("model.safetensors").map(|v| v.as_slice()), Some(&b"w"[..]));
        assert!(s.take("cfc").await.is_empty());
    }
}
