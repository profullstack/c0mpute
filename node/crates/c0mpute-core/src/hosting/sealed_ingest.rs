//! App uploads into private storage: the gateway side.
//!
//! An app (PairUX, anything built on `@profullstack/c0mpute-sealed`) seals an
//! object on the user's device, wrapped only to the user's linked storage
//! identity, and signs the object's hosting record with a fresh per-object
//! key whose seed it wraps to the user as custody
//! ([`c0mpute_sealed::Meta::custody`]). It then streams the ciphertext to a
//! gateway that takes uploads, with a ticket c0mpute.com issued for the
//! user's account. The gateway never holds a key. It checks the ticket (who
//! may upload how much), the record signature, the sealed header's shape and
//! the size, then holds the site and gossips its record exactly as a
//! publisher's own node would, so replication, healing, serving and storage
//! proofs run unchanged over the ciphertext.
//!
//! Routes (merged into [`Hosting::router`]; CORS open, bearer tickets only,
//! never cookies):
//!
//! * `PUT  /storage/v1/sealed/{site_id}?offset=N` append ciphertext at `N`
//!   (resumable: a wrong offset answers 409 with what has arrived)
//! * `GET  /storage/v1/sealed/{site_id}` network status (copies), plus upload
//!   progress when a ticket is sent
//! * `POST /storage/v1/sealed/{site_id}/finish` body `{"record": SiteRecord}`:
//!   verify and publish
//!
//! Off unless the operator names the ticket issuer's ed25519 public key
//! (`[hosting] sealed_ingest_issuer`, or `C0MPUTE_SEALED_INGEST_ISSUER`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as AxPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use c0mpute_proto::Hash;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::site::SiteRecord;
use super::{Hosting, TARGET_REPLICAS};

/// Largest single sealed object an app may upload: a 192 MiB part after
/// Padmé padding, AEAD tags and the header stays under this.
pub const MAX_OBJECT_BYTES: u64 = 200 * 1024 * 1024;
/// Largest body of one PUT.
pub const MAX_PIECE_BYTES: usize = 16 * 1024 * 1024;
/// Domain separation for ticket signatures.
const TICKET_DOMAIN: &str = "c0mpute-sealed-ticket-v1";
/// Staged uploads nobody finished are dropped after this.
const STALE_AFTER: Duration = Duration::from_secs(24 * 3600);
/// How long the gateway keeps re-gossiping a fresh object's record while it
/// has fewer than the target copies (the app is gone by then).
const REGOSSIP_FOR: Duration = Duration::from_secs(30 * 60);

struct Config {
    issuer: c0mpute_net::libp2p::identity::ed25519::PublicKey,
    dir: PathBuf,
}

static CONFIG: RwLock<Option<Arc<Config>>> = RwLock::new(None);
/// Ledger updates are read-modify-write on a small JSON file per upload.
static LEDGER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// At most two objects are hashed and published at once (each up to
/// [`MAX_OBJECT_BYTES`] in memory).
static PUBLISH: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Turn app uploads on (a 64-hex ed25519 issuer key) or off (empty). Staged
/// bytes live under `dir`. Returns false when the key is not usable.
pub fn configure(issuer_hex: &str, dir: PathBuf) -> bool {
    let issuer_hex = issuer_hex.trim();
    let mut slot = match CONFIG.write() {
        Ok(s) => s,
        Err(_) => return false,
    };
    if issuer_hex.is_empty() {
        *slot = None;
        return true;
    }
    let Some(issuer) = hex::decode(issuer_hex)
        .ok()
        .and_then(|b| c0mpute_net::libp2p::identity::ed25519::PublicKey::try_from_bytes(&b).ok())
    else {
        *slot = None;
        return false;
    };
    let fresh = slot.is_none();
    *slot = Some(Arc::new(Config { issuer, dir: dir.clone() }));
    drop(slot);
    if fresh && tokio::runtime::Handle::try_current().is_ok() {
        tokio::spawn(async move {
            loop {
                sweep_stale(&dir).await;
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
    }
    info!("hosting: app uploads into private storage on (/storage/v1/sealed)");
    true
}

fn config() -> Option<Arc<Config>> {
    CONFIG.read().ok().and_then(|c| c.clone())
}

async fn sweep_stale(dir: &Path) {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else { return };
    while let Ok(Some(e)) = rd.next_entry().await {
        let old = e
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_AFTER);
        if old {
            let _ = tokio::fs::remove_dir_all(e.path()).await;
        }
    }
}

/// What c0mpute.com signs for one upload session of one account.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ticket {
    pub v: u8,
    /// Upload session id (also the staging directory name).
    pub upload: String,
    /// Opaque account reference (logs only; never the account id itself).
    #[serde(default)]
    pub sub: String,
    /// Most ciphertext bytes this session may send, all objects together.
    pub max_bytes: u64,
    /// Most objects (sites) this session may create.
    pub max_objects: u32,
    /// Unix seconds.
    pub exp: u64,
}

fn valid_upload_id(s: &str) -> bool {
    (8..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_site_id(s: &str) -> bool {
    s.len() == 20 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Check a `<payload>.<sig>` ticket (both base64url) against the issuer key.
pub fn verify_ticket(raw: &str, issuer: &c0mpute_net::libp2p::identity::ed25519::PublicKey, now: u64) -> Result<Ticket, &'static str> {
    let (payload, sig) = raw.trim().split_once('.').ok_or("malformed ticket")?;
    let sig = URL_SAFE_NO_PAD.decode(sig).map_err(|_| "malformed ticket")?;
    if !issuer.verify(format!("{TICKET_DOMAIN}\n{payload}").as_bytes(), &sig) {
        return Err("ticket signature does not verify");
    }
    let json = URL_SAFE_NO_PAD.decode(payload).map_err(|_| "malformed ticket")?;
    let t: Ticket = serde_json::from_slice(&json).map_err(|_| "malformed ticket")?;
    if t.v != 1 || !valid_upload_id(&t.upload) {
        return Err("unsupported ticket");
    }
    if t.exp < now {
        return Err("ticket expired: ask the app to start the upload again");
    }
    Ok(t)
}

#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    /// Bytes received, all objects together.
    bytes: u64,
    objects: BTreeSet<String>,
    /// Published objects and their sizes.
    published: BTreeMap<String, u64>,
}

fn ledger_path(dir: &Path, upload: &str) -> PathBuf {
    dir.join(upload).join("ledger.json")
}

fn staged_path(dir: &Path, upload: &str, site_id: &str) -> PathBuf {
    dir.join(upload).join(format!("{site_id}.blob"))
}

async fn load_ledger(dir: &Path, upload: &str) -> Ledger {
    match tokio::fs::read(ledger_path(dir, upload)).await {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => Ledger::default(),
    }
}

async fn save_ledger(dir: &Path, upload: &str, l: &Ledger) -> std::io::Result<()> {
    let path = ledger_path(dir, upload);
    if let Some(d) = path.parent() {
        tokio::fs::create_dir_all(d).await?;
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, serde_json::to_vec(l).unwrap_or_default()).await?;
    tokio::fs::rename(&tmp, &path).await
}

fn cors(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, PUT, POST, OPTIONS"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("authorization, content-type"));
    h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn reply(status: StatusCode, body: serde_json::Value) -> Response {
    cors((status, axum::Json(body)).into_response())
}

fn refuse(status: StatusCode, msg: impl Into<String>) -> Response {
    reply(status, serde_json::json!({ "error": msg.into() }))
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
}

/// The config and a verified ticket, or the response that refuses.
fn authorize(headers: &HeaderMap) -> Result<(Arc<Config>, Ticket), Response> {
    let Some(cfg) = config() else {
        return Err(refuse(StatusCode::NOT_FOUND, "this gateway does not take app uploads"));
    };
    let Some(raw) = bearer(headers) else {
        return Err(refuse(StatusCode::UNAUTHORIZED, "send the upload ticket from c0mpute.com as Authorization: Bearer <ticket>"));
    };
    match verify_ticket(raw, &cfg.issuer, now_secs()) {
        Ok(t) => Ok((cfg, t)),
        Err(e) => Err(refuse(StatusCode::UNAUTHORIZED, e)),
    }
}

/// Routes, merged into the hosting router.
pub fn router() -> Router<Hosting> {
    Router::new()
        .route("/storage/v1/sealed/{site_id}", get(status).put(put_piece).options(preflight))
        .route("/storage/v1/sealed/{site_id}/finish", post(finish).options(preflight))
        .layer(DefaultBodyLimit::max(MAX_PIECE_BYTES + 64 * 1024))
}

async fn preflight() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct PutQuery {
    offset: Option<u64>,
}

async fn put_piece(AxPath(site_id): AxPath<String>, Query(q): Query<PutQuery>, headers: HeaderMap, body: Bytes) -> Response {
    let (cfg, t) = match authorize(&headers) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let site_id = site_id.to_ascii_lowercase();
    if !valid_site_id(&site_id) {
        return refuse(StatusCode::BAD_REQUEST, "expected /storage/v1/sealed/<20-hex site id>");
    }
    let Some(offset) = q.offset else {
        return refuse(StatusCode::BAD_REQUEST, "missing ?offset=<bytes already sent>");
    };
    if body.is_empty() {
        return refuse(StatusCode::BAD_REQUEST, "empty body");
    }
    let _g = LEDGER_LOCK.lock().await;
    let mut l = load_ledger(&cfg.dir, &t.upload).await;
    if l.published.contains_key(&site_id) {
        return refuse(StatusCode::CONFLICT, "this object was already published");
    }
    if !l.objects.contains(&site_id) && l.objects.len() >= t.max_objects as usize {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, format!("this upload may create at most {} objects", t.max_objects));
    }
    let path = staged_path(&cfg.dir, &t.upload, &site_id);
    let have = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
    if offset != have {
        return reply(
            StatusCode::CONFLICT,
            serde_json::json!({ "error": "offset does not match what has arrived; resume from `received`", "received": have }),
        );
    }
    if have == 0 && body.len() >= 8 && body[..8] != c0mpute_sealed::format::MAGIC {
        return refuse(StatusCode::BAD_REQUEST, "not a sealed object (c0mpute-private-v1): only ciphertext is accepted");
    }
    if have + body.len() as u64 > MAX_OBJECT_BYTES {
        return refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("one object may be at most {} MiB; split bigger files into parts", MAX_OBJECT_BYTES >> 20),
        );
    }
    if l.bytes + body.len() as u64 > t.max_bytes {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "this upload is over the bytes its ticket allows");
    }
    if let Some(d) = path.parent()
        && let Err(e) = tokio::fs::create_dir_all(d).await
    {
        warn!(err = %e, "sealed ingest: staging dir");
        return refuse(StatusCode::INSUFFICIENT_STORAGE, "staging failed");
    }
    let write = async {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new().create(true).append(true).open(&path).await?;
        f.write_all(&body).await?;
        f.flush().await
    };
    if let Err(e) = write.await {
        warn!(err = %e, "sealed ingest: write");
        return refuse(StatusCode::INSUFFICIENT_STORAGE, "could not stage the bytes");
    }
    l.bytes += body.len() as u64;
    l.objects.insert(site_id);
    if let Err(e) = save_ledger(&cfg.dir, &t.upload, &l).await {
        warn!(err = %e, "sealed ingest: ledger");
    }
    reply(StatusCode::OK, serde_json::json!({ "received": have + body.len() as u64 }))
}

async fn network_status(h: &Hosting, site_id: &str) -> serde_json::Value {
    let rec = h.record_for(site_id).await;
    let copies = h.holders(site_id).await.len();
    serde_json::json!({
        "site_id": site_id,
        "ref": crate::sealed_store::plain_ref(site_id),
        "published": rec.is_some(),
        "seq": rec.as_ref().map(|r| r.seq),
        "size": rec.as_ref().map(|r| r.size),
        "copies": copies,
        "target": TARGET_REPLICAS,
    })
}

async fn status(State(h): State<Hosting>, AxPath(site_id): AxPath<String>, headers: HeaderMap) -> Response {
    let site_id = site_id.to_ascii_lowercase();
    if !valid_site_id(&site_id) {
        return refuse(StatusCode::BAD_REQUEST, "expected /storage/v1/sealed/<20-hex site id>");
    }
    let mut body = network_status(&h, &site_id).await;
    // Upload progress for the app that is sending it (to resume).
    if bearer(&headers).is_some()
        && let Ok((cfg, t)) = authorize(&headers)
    {
        let l = load_ledger(&cfg.dir, &t.upload).await;
        let received = match l.published.get(&site_id) {
            Some(n) => *n,
            None => tokio::fs::metadata(staged_path(&cfg.dir, &t.upload, &site_id)).await.map(|m| m.len()).unwrap_or(0),
        };
        body["received"] = received.into();
        body["finished"] = l.published.contains_key(&site_id).into();
    }
    reply(StatusCode::OK, body)
}

#[derive(Deserialize)]
struct FinishBody {
    record: SiteRecord,
}

/// The sealed format's fixed shape, without any key: magic, slot count, and
/// a stream of whole AES-GCM chunks ending in a non-empty last one.
pub fn sealed_shape_ok(blob: &[u8]) -> bool {
    const TAG: usize = 16;
    let chunk = c0mpute_sealed::CHUNK_PLAINTEXT + TAG;
    let h = c0mpute_sealed::HEADER_LEN;
    if blob.len() < h + TAG + 1 || blob[..8] != c0mpute_sealed::format::MAGIC {
        return false;
    }
    if u16::from_be_bytes([blob[56], blob[57]]) as usize != c0mpute_sealed::MAX_RECIPIENTS {
        return false;
    }
    let stream = blob.len() - h;
    let n = stream.div_ceil(chunk);
    stream - (n - 1) * chunk > TAG
}

async fn finish(State(h): State<Hosting>, AxPath(site_id): AxPath<String>, headers: HeaderMap, body: Bytes) -> Response {
    let (cfg, t) = match authorize(&headers) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let site_id = site_id.to_ascii_lowercase();
    if !valid_site_id(&site_id) {
        return refuse(StatusCode::BAD_REQUEST, "expected /storage/v1/sealed/<20-hex site id>/finish");
    }
    let Ok(FinishBody { record }) = serde_json::from_slice::<FinishBody>(&body) else {
        return refuse(StatusCode::BAD_REQUEST, "body must be {\"record\": <signed site record>}");
    };
    if record.v != 1 || !record.domains.is_empty() || record.site_id != site_id {
        return refuse(StatusCode::BAD_REQUEST, "record must be a plain v1 record for this site id, without domains");
    }
    if !record.verify() {
        return refuse(StatusCode::FORBIDDEN, "record signature does not verify");
    }
    let path = staged_path(&cfg.dir, &t.upload, &site_id);
    {
        let _g = LEDGER_LOCK.lock().await;
        let l = load_ledger(&cfg.dir, &t.upload).await;
        if l.published.contains_key(&site_id) {
            return reply(StatusCode::OK, network_status(&h, &site_id).await);
        }
        if !l.objects.contains(&site_id) {
            return refuse(StatusCode::NOT_FOUND, "nothing was uploaded for this object under this ticket");
        }
    }
    let _permit = match PUBLISH.acquire().await {
        Ok(p) => p,
        Err(_) => return refuse(StatusCode::SERVICE_UNAVAILABLE, "shutting down"),
    };
    let blob = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return refuse(StatusCode::NOT_FOUND, "nothing staged for this object"),
    };
    if blob.len() as u64 != record.size {
        return refuse(
            StatusCode::BAD_REQUEST,
            format!("record says {} bytes but {} arrived; upload the rest first", record.size, blob.len()),
        );
    }
    if !sealed_shape_ok(&blob) {
        return refuse(StatusCode::BAD_REQUEST, "not a well-formed sealed object (c0mpute-private-v1)");
    }
    let built = crate::sealed_store::built_site(&record.name, &blob);
    drop(blob);
    if Hash::of(&built.manifest.to_bytes()).to_hex() != record.manifest {
        return refuse(
            StatusCode::BAD_REQUEST,
            "record does not sign this object's manifest (one file \"o\", application/octet-stream, 1 MiB blake3 chunks)",
        );
    }
    if let Some(cur) = h.record_for(&site_id).await
        && cur.seq >= record.seq
    {
        return refuse(StatusCode::CONFLICT, "an equal or newer version of this object is already on the network");
    }
    if let Err(e) = h.publish_site(&built, record.clone()).await {
        return refuse(StatusCode::CONFLICT, format!("publish refused: {e:#}"));
    }
    drop(built);
    {
        let _g = LEDGER_LOCK.lock().await;
        let mut l = load_ledger(&cfg.dir, &t.upload).await;
        l.published.insert(site_id.clone(), record.size);
        let _ = save_ledger(&cfg.dir, &t.upload, &l).await;
    }
    let _ = tokio::fs::remove_file(&path).await;
    info!(site = %site_id, bytes = record.size, upload = %t.upload, "sealed ingest: published an app upload");
    // The app does not stay around to re-gossip: this gateway does, until
    // the object has its copies.
    let h2 = h.clone();
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        while started.elapsed() < REGOSSIP_FOR {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if h2.holders(&record.site_id).await.len() >= TARGET_REPLICAS {
                break;
            }
            h2.reannounce(&record).await;
        }
    });
    let mut body = network_status(&h, &site_id).await;
    body["status"] = "published".into();
    reply(StatusCode::OK, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use c0mpute_net::libp2p::identity::Keypair;

    fn ticket(k: &Keypair, t: &Ticket) -> String {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(t).unwrap());
        let sig = k.sign(format!("{TICKET_DOMAIN}\n{payload}").as_bytes()).unwrap();
        format!("{payload}.{}", URL_SAFE_NO_PAD.encode(sig))
    }

    fn issuer(k: &Keypair) -> c0mpute_net::libp2p::identity::ed25519::PublicKey {
        k.public().try_into_ed25519().unwrap()
    }

    #[test]
    fn tickets_verify_and_expire() {
        let k = Keypair::generate_ed25519();
        let t = Ticket { v: 1, upload: "up_0123456789".into(), sub: "x".into(), max_bytes: 10, max_objects: 1, exp: 1000 };
        let raw = ticket(&k, &t);
        assert_eq!(verify_ticket(&raw, &issuer(&k), 999).unwrap().upload, "up_0123456789");
        assert!(verify_ticket(&raw, &issuer(&k), 1001).is_err(), "expired");
        let other = Keypair::generate_ed25519();
        assert!(verify_ticket(&raw, &issuer(&other), 999).is_err(), "wrong issuer");
        // A payload swapped under the old signature fails.
        let mut t2 = t.clone();
        t2.max_bytes = 1 << 40;
        let forged = format!("{}.{}", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&t2).unwrap()), raw.split_once('.').unwrap().1);
        assert!(verify_ticket(&forged, &issuer(&k), 999).is_err());
        let bad_id = Ticket { upload: "../../etc".into(), ..t };
        assert!(verify_ticket(&ticket(&k, &bad_id), &issuer(&k), 999).is_err());
    }

    #[test]
    fn sealed_shape_is_checked_without_a_key() {
        let owner = c0mpute_sealed::StorageSecret::generate();
        let sealed = c0mpute_sealed::seal(
            &vec![5u8; 200_000],
            &c0mpute_sealed::SealParams {
                kind: c0mpute_sealed::Kind::File,
                name: "n",
                owner: &owner,
                file_name: None,
                content_type: None,
                files: Default::default(),
                parts: vec![],
                recipients: &[owner.public()],
                created: 0,
                version: 0,
            },
        )
        .unwrap();
        assert!(sealed_shape_ok(&sealed.blob));
        // Cut so the last chunk is shorter than its tag.
        let cut = c0mpute_sealed::HEADER_LEN + c0mpute_sealed::CHUNK_PLAINTEXT + 16 + 10;
        assert!(!sealed_shape_ok(&sealed.blob[..cut]), "cut inside the tag");
        assert!(!sealed_shape_ok(b"plaintext, not sealed"));
        let mut bad = sealed.blob.clone();
        bad[0] ^= 1;
        assert!(!sealed_shape_ok(&bad));
    }
}
