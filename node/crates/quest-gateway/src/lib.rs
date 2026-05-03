//! HTTP gateway role: takes browser requests for `/chunks/<hash>` or HLS
//! manifests and serves bytes either from the local `ChunkStore` or by
//! fetching them through the `Network` layer.
//!
//! The gateway is deliberately stateless beyond the local cache — every
//! request is content-addressed, so any gateway can serve any chunk.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use quest_net::Network;
use quest_proto::{ChunkRequest, Hash};
use quest_store::ChunkStore;
use tracing::{info, warn};

#[derive(Clone)]
pub struct GatewayState {
    pub store: ChunkStore,
    pub net: Arc<dyn Network>,
}

pub fn router(state: GatewayState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/chunks/:hash", get(chunk_handler))
        .with_state(state)
}

pub async fn serve(state: GatewayState, addr: SocketAddr) -> Result<()> {
    let app = router(state);
    info!(%addr, "gateway listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn chunk_handler(
    State(state): State<GatewayState>,
    Path(hash_hex): Path<String>,
) -> impl IntoResponse {
    let hash = match Hash::from_hex(&hash_hex) {
        Ok(h) => h,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "invalid hash").into_response();
        }
    };

    if state.store.has(&hash).await {
        match state.store.get(&hash).await {
            Ok(bytes) => return chunk_response(bytes).into_response(),
            Err(e) => {
                warn!(hash = %hash, err = %e, "local chunk failed verification");
            }
        }
    }

    match state
        .net
        .fetch(&ChunkRequest {
            chunk_hash: hash,
            shard_index: None,
        })
        .await
    {
        Ok(bytes) => {
            let actual = Hash::of(&bytes);
            if actual != hash {
                return (
                    StatusCode::BAD_GATEWAY,
                    "fetched bytes failed integrity check",
                )
                    .into_response();
            }
            // Best-effort cache; ignore errors so we still serve.
            let _ = state.store.put(&bytes).await;
            chunk_response(bytes).into_response()
        }
        Err(e) => {
            warn!(hash = %hash, err = %e, "chunk fetch failed");
            (StatusCode::NOT_FOUND, "chunk not found").into_response()
        }
    }
}

fn chunk_response(bytes: Vec<u8>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "video/mp2t"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
}
