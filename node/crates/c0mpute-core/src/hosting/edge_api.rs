//! Verifiable site reads for clients that do not trust the gateway they use.
//!
//! Any node can be a public gateway, so an edge (the Moshpit c0mpute edge,
//! for one) fetching from a stranger's gateway must be able to check what it
//! gets. A site is already self-certifying: the owner signs a [`SiteRecord`]
//! pinning a manifest by blake3, the manifest pins every file's chunks by
//! blake3, and the site id is derived from the owner key and name. These two
//! routes hand a client exactly the pieces it needs to check one request:
//!
//! - `GET /hosting/v1/sites/{site}/resolve?path=/a/b` answers
//!   `{v: 1, record, manifest, parts, path, status, entry}`:
//!   `record` is the signed record as gossiped; `manifest` is the base64 bytes
//!   of the chunk `record.manifest` names; `parts` holds, for a large (v2)
//!   site, the base64 bytes of every manifest part consulted (so a client can
//!   also check that earlier candidates were absent); `path` is the manifest
//!   path served (`null` when nothing matched), `status` 200 or 404 (when
//!   `404.html` stands in), `entry` its [`site::FileEntry`].
//! - `GET /hosting/v1/sites/{site}/chunks/{hash}` answers one chunk's bytes,
//!   from this node or the site's holders, only when the hash belongs to the
//!   site's current manifest (no open relay).
//!
//! A client checks: the record signature over `signing_bytes` with `owner`,
//! `site_id == site_id_for(owner, name)`, `blake3(manifest) == record.manifest`,
//! each part against its `PartRef`, the resolution against
//! [`site::candidates`], then `blake3(chunk) == hash` for every chunk.

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use c0mpute_proto::Hash;
use serde::Deserialize;
use serde_json::json;

use super::Hosting;
use super::site::{self, Manifest, ManifestPart};

/// Most candidate paths a request can expand to (`site::candidates` gives at
/// most three) plus `404.html`.
const MAX_CONSULTED: usize = 4;

#[derive(Debug, Deserialize)]
pub(super) struct ResolveQuery {
    #[serde(default)]
    path: String,
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unavailable() -> Response {
    (StatusCode::BAD_GATEWAY, "site is unavailable right now").into_response()
}

impl Hosting {
    /// The raw bytes of the manifest chunk a record names.
    async fn manifest_bytes(&self, rec: &site::SiteRecord) -> anyhow::Result<Vec<u8>> {
        let hash = Hash::from_hex(&rec.manifest).map_err(|_| anyhow::anyhow!("manifest hash"))?;
        let holders = self.0.map.read().await.fetch_sources(&rec.site_id);
        self.fetch_chunk(&hash, &holders).await
    }

    /// The raw bytes of one manifest part, checked against its reference.
    async fn part_bytes(&self, rec: &site::SiteRecord, r: &site::PartRef) -> anyhow::Result<Vec<u8>> {
        let hash = Hash::from_hex(&r.hash).map_err(|_| anyhow::anyhow!("part hash"))?;
        let holders = self.0.map.read().await.fetch_sources(&rec.site_id);
        let bytes = self.fetch_chunk(&hash, &holders).await?;
        ManifestPart::from_bytes(&bytes, r)?;
        Ok(bytes)
    }
}

/// `GET /hosting/v1/sites/{site}/resolve?path=...`
pub(super) async fn resolve(
    State(h): State<Hosting>,
    AxPath(site_id): AxPath<String>,
    Query(q): Query<ResolveQuery>,
) -> Response {
    let Some(rec) = h.lookup_record(&site_id).await else {
        return (StatusCode::NOT_FOUND, "no such site on the network (yet)").into_response();
    };
    let Ok(mbytes) = h.manifest_bytes(&rec).await else {
        return unavailable();
    };
    let Ok(manifest) = Manifest::from_bytes(&mbytes) else {
        return unavailable();
    };
    let mut consulted: Vec<String> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    let mut found: Option<(String, site::FileEntry, u16)> = None;
    let candidates = site::candidates(&q.path)
        .into_iter()
        .map(|c| (c, 200u16))
        .chain(std::iter::once(("404.html".to_string(), 404u16)))
        .take(MAX_CONSULTED);
    for (path, status) in candidates {
        let entry = match &manifest {
            Manifest::Inline(m) => m.files.get(&path).cloned(),
            Manifest::Parted(_) => {
                let Some(pr) = manifest.part_for(&path) else { continue };
                let Ok(pb) = h.part_bytes(&rec, pr).await else {
                    return unavailable();
                };
                if !consulted.contains(&pr.hash) {
                    consulted.push(pr.hash.clone());
                    parts.push(b64(&pb));
                }
                ManifestPart::from_bytes(&pb, pr).ok().and_then(|p| p.files.get(&path).cloned())
            }
        };
        if let Some(e) = entry {
            found = Some((path, e, status));
            break;
        }
    }
    let (path, entry, status) = match found {
        Some((p, e, s)) => (Some(p), Some(e), s),
        None => (None, None, 404),
    };
    let mut resp = axum::Json(json!({
        "v": 1,
        "record": rec,
        "manifest": b64(&mbytes),
        "parts": parts,
        "path": path,
        "status": status,
        "entry": entry,
    }))
    .into_response();
    // The record can move on; keep caches short.
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=30"));
    resp
}

/// `GET /hosting/v1/sites/{site}/chunks/{hash}`: one chunk of the site's
/// current version (content-addressed, so cacheable forever).
pub(super) async fn chunk(
    State(h): State<Hosting>,
    AxPath((site_id, hash_hex)): AxPath<(String, String)>,
) -> Response {
    let Ok(hash) = Hash::from_hex(&hash_hex) else {
        return (StatusCode::BAD_REQUEST, "not a blake3 hash").into_response();
    };
    let Some(rec) = h.lookup_record(&site_id).await else {
        return (StatusCode::NOT_FOUND, "no such site on the network (yet)").into_response();
    };
    // Only chunks of this site: the manifest, a part, or a file chunk.
    let belongs = if rec.manifest == hash_hex {
        true
    } else {
        match h.manifest(&rec).await {
            Ok(m) => match m.as_ref() {
                Manifest::Inline(sm) => sm.files.values().any(|f| f.chunks.iter().any(|c| *c == hash_hex)),
                Manifest::Parted(root) => {
                    let mut hit = root.parts.iter().any(|p| p.hash == hash_hex);
                    for pr in &root.parts {
                        if hit {
                            break;
                        }
                        if let Ok(p) = h.part(&rec.site_id, pr).await {
                            hit = p.files.values().any(|f| f.chunks.iter().any(|c| *c == hash_hex));
                        }
                    }
                    hit
                }
            },
            Err(_) => return unavailable(),
        }
    };
    if !belongs {
        return (StatusCode::NOT_FOUND, "not a chunk of this site").into_response();
    }
    let holders = h.0.map.read().await.fetch_sources(&rec.site_id);
    match h.fetch_chunk(&hash, &holders).await {
        Ok(bytes) => {
            let mut resp = (StatusCode::OK, bytes).into_response();
            let hdr = resp.headers_mut();
            hdr.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
            hdr.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
            resp
        }
        Err(_) => unavailable(),
    }
}
