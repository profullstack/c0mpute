//! Hosted RTMP ingest: `c0mpute stream ingest --listen 0.0.0.0:1935`.
//!
//! OBS, PairUX or `ffmpeg -f flv` publish to `rtmp://<host>/live/<stream_key>`.
//! For each publisher this node:
//!
//!   1. speaks the RTMP handshake and chunk stream (`rml_rtmp`), and on the
//!      `publish` command asks c0mpute.com whether the key is good
//!      (`POST /api/v1/live/ingest/auth`). A key that is refused, malformed,
//!      or cannot be checked is rejected before any media is read and before
//!      any ffmpeg is started ([`Gate::admit`]).
//!   2. wraps the audio and video messages as FLV and pipes them into one
//!      ffmpeg (`broadcast::run`, stdin input), which cuts the HLS segments
//!      the rest of the live plugin already swarms. The stream is signed
//!      with this node's broadcaster key under a network name derived from
//!      the account and the stream's name ([`network_name`]), with the
//!      account binding (`account`, `paid`, `max_viewers`) in every record.
//!   3. tells c0mpute.com the stream started (with the network stream id and
//!      this node's peer id) and, when the publisher goes away, that it
//!      ended (`POST /api/v1/live/ingest/event`).
//!   4. asks again every minute: a key that stopped working ends the stream,
//!      and a stream whose account ran out of balance drops to the free tier
//!      (its records say `paid: false` from then on).
//!
//! Not a default role: it needs a public port and ffmpeg. One process serves
//! many publishers at once, each with its own ffmpeg.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use super::broadcast::{self, Codec, Feed, GoOptions};
use super::record::{self, Binding};
use super::{LiveStream, Timing, meter};

/// The RTMP application publishers connect to (`rtmp://host/live/<key>`).
pub const APP: &str = "live";
/// Environment variable holding the shared secret for the ingest API.
pub const SECRET_ENV: &str = "C0MPUTE_LIVE_INGEST_SECRET";
/// Publishers served at once.
const MAX_CONNECTIONS: usize = 256;
/// A connection that sends nothing for this long is dropped.
const IDLE: Duration = Duration::from_secs(30);
/// How long a publisher may take from connecting to publishing.
const SETUP: Duration = Duration::from_secs(20);
/// How often a live stream's key and tier are checked again.
const RECHECK: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct IngestOptions {
    pub listen: SocketAddr,
    /// c0mpute.com origin, e.g. `https://c0mpute.com`.
    pub api: String,
    /// Shared secret sent as `Authorization: Bearer`.
    pub secret: String,
    pub segment_secs: u32,
    pub window: usize,
    /// After a publisher leaves, keep its last window up this long.
    pub linger: Duration,
}

/// c0mpute.com's answer for a stream key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthReply {
    /// Public stream id, `lv_…`.
    pub id: String,
    /// The owning account's id.
    pub account: String,
    /// The stream's name, unique within the account.
    pub name: String,
    pub paid: bool,
    pub max_height: Option<u32>,
    pub max_viewers: Option<u32>,
}

impl AuthReply {
    /// Parse `{"ok":true,"id":…,"account":…,"name":…,"paid":…,
    /// "max_height":…|null,"max_viewers":…|null}`. `None` unless `ok` is true
    /// and the id, account and name are there.
    pub fn parse(v: &serde_json::Value) -> Option<Self> {
        if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
            return None;
        }
        let text = |k: &str| match v.get(k) {
            Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(serde_json::Value::Number(n)) => Some(n.to_string()),
            _ => None,
        };
        let num = |k: &str| v.get(k).and_then(|n| n.as_u64()).and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0);
        Some(AuthReply {
            id: text("id")?,
            account: text("account")?,
            name: text("name")?,
            paid: v.get("paid").and_then(|p| p.as_bool()).unwrap_or(false),
            max_height: num("max_height"),
            max_viewers: num("max_viewers"),
        })
    }

    pub fn binding(&self) -> Binding {
        Binding {
            account: self.account.clone(),
            paid: self.paid,
            max_viewers: if self.paid { None } else { self.max_viewers },
        }
    }
}

/// The two ingest calls on c0mpute.com.
pub struct Api {
    base: String,
    secret: String,
    http: reqwest::Client,
}

impl Api {
    pub fn new(base: &str, secret: &str) -> Result<Self> {
        if secret.trim().is_empty() {
            bail!("{SECRET_ENV} is empty; the ingest needs the shared secret");
        }
        Ok(Api {
            base: base.trim_end_matches('/').to_string(),
            secret: secret.trim().to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .context("build http client")?,
        })
    }

    /// `POST /api/v1/live/ingest/auth {"stream_key":…}`. `Ok(None)`: the key
    /// is refused (403, or `ok: false`). `Err`: the answer could not be had.
    pub async fn auth(&self, stream_key: &str) -> Result<Option<AuthReply>> {
        let url = format!("{}/api/v1/live/ingest/auth", self.base);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.secret)
            .json(&json!({ "stream_key": stream_key }))
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::FORBIDDEN {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("POST {url}: {status}");
        }
        let v: serde_json::Value = resp.json().await.with_context(|| format!("POST {url}: not JSON"))?;
        Ok(AuthReply::parse(&v))
    }

    /// `POST /api/v1/live/ingest/event {"id":…,"event":"start"|"end",
    /// "stream_id":…,"peer":…}`.
    pub async fn event(&self, id: &str, event: &str, stream_id: &str, peer: &str) -> Result<()> {
        let url = format!("{}/api/v1/live/ingest/event", self.base);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.secret)
            .json(&json!({ "id": id, "event": event, "stream_id": stream_id, "peer": peer }))
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        if !resp.status().is_success() {
            bail!("POST {url}: {}", resp.status());
        }
        Ok(())
    }
}

/// A stream key as c0mpute.com issues them: `c0l_` and 4-124 of a-z, A-Z,
/// 0-9, `_`, `-`. Anything else is refused without asking the API.
pub fn valid_stream_key(key: &str) -> bool {
    key.len() >= 8
        && key.len() <= 128
        && key.starts_with("c0l_")
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The name a hosted stream is broadcast under on the network. The contract
/// is "`<account>/<name>`, so two accounts cannot collide"; a network stream
/// name is 1-63 of a-z, 0-9 and `-` (nodes reject anything else, including
/// `/`), so this is the readable part of the stream's name followed by 16
/// hex characters of blake3(`account` + `/` + `name`). Same account and
/// name: same network name and, with one ingest key, the same stream id.
pub fn network_name(account: &str, name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "stream" } else { slug };
    let tag = c0mpute_proto::Hash::of(format!("c0mpute-live-account\n{account}/{name}").as_bytes()).to_hex();
    format!("{slug}-{}", &tag[..16])
}

/// A publisher that may go live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admission {
    /// Public stream id on c0mpute.com (`lv_…`).
    pub id: String,
    pub stream_key: String,
    /// Network stream name ([`network_name`]).
    pub name: String,
    /// Network stream id (hash of this ingest's owner key and the name).
    pub stream_id: String,
    pub binding: Binding,
    pub max_height: Option<u32>,
}

/// Decides who may publish. Holds the API client, this ingest's owner key
/// (stream ids derive from it) and which streams are live here.
pub struct Gate {
    pub api: Api,
    owner_hex: String,
    active: std::sync::Mutex<HashSet<String>>,
}

impl Gate {
    pub fn new(api: Api, owner_hex: String) -> Self {
        Gate {
            api,
            owner_hex,
            active: std::sync::Mutex::new(HashSet::new()),
        }
    }

    /// Check a `publish` to `rtmp://host/<app>/<key>`. `Err` is the reason
    /// to give the publisher; nothing was started.
    pub async fn admit(&self, app: &str, key: &str) -> std::result::Result<Admission, String> {
        if app.trim_matches('/') != APP {
            return Err(format!("publish to rtmp://<host>/{APP}/<stream_key>"));
        }
        // Some encoders append `?query` to the key.
        let key = key.split('?').next().unwrap_or("").trim();
        if !valid_stream_key(key) {
            return Err("bad stream key".into());
        }
        let reply = match self.api.auth(key).await {
            Ok(Some(r)) => r,
            Ok(None) => return Err("stream key refused".into()),
            Err(e) => {
                warn!(err = %format!("{e:#}"), "live ingest: could not check a stream key");
                return Err("the ingest cannot check stream keys right now; try again".into());
            }
        };
        let binding = reply.binding();
        if !binding.valid() {
            return Err("stream key refused".into());
        }
        let name = network_name(&reply.account, &reply.name);
        let stream_id = record::stream_id_for(&self.owner_hex, &name);
        if !self.active.lock().unwrap_or_else(|e| e.into_inner()).insert(name.clone()) {
            return Err("this stream is already live; stop the other encoder first".into());
        }
        Ok(Admission {
            id: reply.id,
            stream_key: key.to_string(),
            name,
            stream_id,
            binding,
            max_height: reply.max_height,
        })
    }

    /// The stream is over: its name may be published again.
    pub fn release(&self, name: &str) {
        self.active.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
    }

    pub fn live(&self) -> usize {
        self.active.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// Where an admitted publisher's media goes.
pub struct Publishing {
    /// FLV bytes (header first, then tags).
    pub tx: mpsc::Sender<Bytes>,
    /// Resolves (sender dropped) when the stream is over on our side.
    pub done: oneshot::Receiver<()>,
}

/// Starts the thing that consumes an admitted publisher's FLV. The real one
/// runs ffmpeg ([`FfmpegSink`]); tests collect the bytes.
pub trait Sink: Send + Sync + 'static {
    fn start(&self, admission: Admission) -> Publishing;
}

/// The 13 bytes an FLV stream starts with: header (audio + video) and the
/// first, zero, previous-tag size.
pub fn flv_header() -> [u8; 13] {
    [b'F', b'L', b'V', 1, 0x05, 0, 0, 0, 9, 0, 0, 0, 0]
}

/// One FLV tag: `kind` 8 (audio), 9 (video) or 18 (script data), the RTMP
/// timestamp in ms, the RTMP message payload, then the tag's size.
pub fn flv_tag(kind: u8, timestamp_ms: u32, data: &[u8]) -> Vec<u8> {
    let len = data.len().min(0x00ff_ffff);
    let mut out = Vec::with_capacity(len + 15);
    out.push(kind);
    out.extend_from_slice(&(len as u32).to_be_bytes()[1..]);
    out.extend_from_slice(&timestamp_ms.to_be_bytes()[1..]);
    out.push((timestamp_ms >> 24) as u8);
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&data[..len]);
    out.extend_from_slice(&((len + 11) as u32).to_be_bytes());
    out
}

fn rtmp_err<E: std::fmt::Debug>(e: E) -> anyhow::Error {
    anyhow!("rtmp: {e:?}")
}

async fn read_some(sock: &mut TcpStream, buf: &mut [u8], limit: Duration) -> Result<usize> {
    match tokio::time::timeout(limit, sock.read(buf)).await {
        Ok(r) => r.context("read"),
        Err(_) => bail!("publisher went quiet"),
    }
}

/// Serve one RTMP connection to the end.
pub async fn handle_conn<S: Sink>(mut sock: TcpStream, gate: Arc<Gate>, sink: Arc<S>) -> Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    let started = tokio::time::Instant::now();

    let mut handshake = Handshake::new(PeerType::Server);
    let mut input: Vec<u8> = loop {
        let n = read_some(&mut sock, &mut buf, SETUP).await?;
        if n == 0 {
            return Ok(());
        }
        match handshake.process_bytes(&buf[..n]).map_err(rtmp_err)? {
            HandshakeProcessResult::InProgress { response_bytes } => sock.write_all(&response_bytes).await?,
            HandshakeProcessResult::Completed {
                response_bytes,
                remaining_bytes,
            } => {
                sock.write_all(&response_bytes).await?;
                break remaining_bytes;
            }
        }
    };

    let (mut session, initial) = ServerSession::new(ServerSessionConfig::new()).map_err(rtmp_err)?;
    let mut pending: VecDeque<ServerSessionResult> = initial.into();
    let mut publishing: Option<Publishing> = None;

    'conn: loop {
        if !input.is_empty() {
            pending.extend(session.handle_input(&input).map_err(rtmp_err)?);
            input.clear();
        }
        while let Some(result) = pending.pop_front() {
            let event = match result {
                ServerSessionResult::OutboundResponse(packet) => {
                    sock.write_all(&packet.bytes).await?;
                    continue;
                }
                ServerSessionResult::RaisedEvent(event) => event,
                ServerSessionResult::UnhandleableMessageReceived(_) => continue,
            };
            match event {
                ServerSessionEvent::ConnectionRequested { request_id, app_name } => {
                    if app_name.trim_matches('/') == APP {
                        pending.extend(session.accept_request(request_id).map_err(rtmp_err)?);
                    } else {
                        for r in session
                            .reject_request(request_id, "NetConnection.Connect.Rejected", "unknown application")
                            .map_err(rtmp_err)?
                        {
                            if let ServerSessionResult::OutboundResponse(p) = r {
                                sock.write_all(&p.bytes).await?;
                            }
                        }
                        break 'conn;
                    }
                }
                ServerSessionEvent::PublishStreamRequested {
                    request_id,
                    app_name,
                    stream_key,
                    ..
                } => {
                    let verdict = if publishing.is_some() {
                        Err("one stream per connection".to_string())
                    } else {
                        gate.admit(&app_name, &stream_key).await
                    };
                    match verdict {
                        Ok(admission) => {
                            info!(stream = %admission.stream_id, id = %admission.id, paid = admission.binding.paid, "live ingest: publisher admitted");
                            pending.extend(session.accept_request(request_id).map_err(rtmp_err)?);
                            let p = sink.start(admission);
                            if p.tx.send(Bytes::copy_from_slice(&flv_header())).await.is_err() {
                                break 'conn;
                            }
                            publishing = Some(p);
                        }
                        Err(reason) => {
                            // Refused: say why, then hang up. Nothing was
                            // started and no media was read.
                            info!(%reason, "live ingest: publish refused");
                            for r in session
                                .reject_request(request_id, "NetStream.Publish.BadName", &reason)
                                .map_err(rtmp_err)?
                            {
                                if let ServerSessionResult::OutboundResponse(p) = r {
                                    sock.write_all(&p.bytes).await?;
                                }
                            }
                            let _ = sock.flush().await;
                            break 'conn;
                        }
                    }
                }
                ServerSessionEvent::AudioDataReceived { data, timestamp, .. } => {
                    if !forward(&publishing, flv_tag(8, timestamp.value, &data)).await {
                        break 'conn;
                    }
                }
                ServerSessionEvent::VideoDataReceived { data, timestamp, .. } => {
                    if !forward(&publishing, flv_tag(9, timestamp.value, &data)).await {
                        break 'conn;
                    }
                }
                ServerSessionEvent::PublishStreamFinished { .. } => break 'conn,
                ServerSessionEvent::PlayStreamRequested { request_id, .. } => {
                    // Ingest only: watch on a gateway, not here.
                    for r in session
                        .reject_request(request_id, "NetStream.Play.Failed", "this server only takes publishes")
                        .map_err(rtmp_err)?
                    {
                        if let ServerSessionResult::OutboundResponse(p) = r {
                            sock.write_all(&p.bytes).await?;
                        }
                    }
                    break 'conn;
                }
                _ => {}
            }
        }
        if publishing.is_none() && started.elapsed() > SETUP {
            bail!("no publish within {} s", SETUP.as_secs());
        }
        let n = tokio::select! {
            n = read_some(&mut sock, &mut buf, IDLE) => n?,
            _ = stream_over(&mut publishing) => break 'conn,
        };
        if n == 0 {
            break 'conn;
        }
        input.extend_from_slice(&buf[..n]);
    }
    // Dropping the sender closes ffmpeg's stdin: the stream ends.
    drop(publishing);
    let _ = sock.shutdown().await;
    Ok(())
}

/// Resolves when the sink says the stream is over; never before a publish.
async fn stream_over(publishing: &mut Option<Publishing>) {
    match publishing {
        Some(p) => {
            let _ = (&mut p.done).await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Hand one FLV tag to the sink. False when the stream is over, the sink is
/// stuck, or media arrived before a publish was admitted.
async fn forward(publishing: &Option<Publishing>, tag: Vec<u8>) -> bool {
    let Some(p) = publishing else { return false };
    matches!(
        tokio::time::timeout(Duration::from_secs(15), p.tx.send(Bytes::from(tag))).await,
        Ok(Ok(()))
    )
}

/// Accept publishers forever.
pub async fn accept_loop<S: Sink>(listener: TcpListener, gate: Arc<Gate>, sink: Arc<S>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!(err = %e, "live ingest: accept failed");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            debug!(%peer, "live ingest: full; connection dropped");
            continue;
        };
        let _ = sock.set_nodelay(true);
        let (gate, sink) = (gate.clone(), sink.clone());
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = handle_conn(sock, gate, sink).await {
                debug!(%peer, err = %format!("{e:#}"), "live ingest: connection ended");
            }
        });
    }
}

/// The real sink: one ffmpeg per publisher through [`broadcast::run`].
pub struct FfmpegSink {
    pub gate: Arc<Gate>,
    pub session: broadcast::Session,
    /// This node's peer id, reported with the start event.
    pub peer: String,
    pub segment_secs: u32,
    pub window: usize,
    pub linger: Duration,
}

impl Sink for FfmpegSink {
    fn start(&self, admission: Admission) -> Publishing {
        let (tx, rx) = mpsc::channel::<Bytes>(512);
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let (bind_tx, bind_rx) = watch::channel(Some(admission.binding.clone()));
        let (stop_tx, stop_rx) = watch::channel(false);
        let opts = GoOptions {
            input: "-".into(),
            name: admission.name.clone(),
            format: Some("flv".into()),
            segment_secs: self.segment_secs,
            window: self.window,
            low: false,
            // Copied as published unless the tier caps the height.
            codec: Codec::Copy,
            listen: false,
            bind: None,
            linger: self.linger,
            reconnect: false,
            max_height: admission.max_height,
        };
        let feed = Feed {
            stdin: Some(rx),
            binding: Some(bind_rx),
            stop: Some(stop_rx),
            quiet: true,
        };
        let (gate, session, peer) = (self.gate.clone(), self.session.clone(), self.peer.clone());
        tokio::spawn(async move {
            if let Err(e) = gate.api.event(&admission.id, "start", &admission.stream_id, &peer).await {
                warn!(id = %admission.id, err = %format!("{e:#}"), "live ingest: start event not delivered");
            }
            // Keep the tier current, and end the stream when its key dies.
            let recheck = {
                let (gate, admission) = (gate.clone(), admission.clone());
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(RECHECK).await;
                        match gate.api.auth(&admission.stream_key).await {
                            Ok(Some(r)) if r.id == admission.id => {
                                let now = Some(r.binding());
                                bind_tx.send_if_modified(|cur| {
                                    let changed = *cur != now;
                                    if changed {
                                        info!(id = %admission.id, paid = r.paid, "live ingest: tier changed");
                                        *cur = now.clone();
                                    }
                                    changed
                                });
                            }
                            Ok(_) => {
                                info!(id = %admission.id, "live ingest: stream key no longer valid; ending the stream");
                                let _ = stop_tx.send(true);
                                return;
                            }
                            // c0mpute.com unreachable: keep the stream up.
                            Err(e) => debug!(err = %format!("{e:#}"), "live ingest: recheck failed"),
                        }
                    }
                })
            };
            let outcome = broadcast::run(&session, &opts, feed).await;
            recheck.abort();
            match &outcome {
                Ok(()) => info!(id = %admission.id, stream = %admission.stream_id, "live ingest: stream ended"),
                Err(e) => warn!(id = %admission.id, err = %format!("{e:#}"), "live ingest: stream failed"),
            }
            if let Err(e) = gate.api.event(&admission.id, "end", &admission.stream_id, &peer).await {
                warn!(id = %admission.id, err = %format!("{e:#}"), "live ingest: end event not delivered");
            }
            gate.release(&admission.name);
            drop(done_tx);
        });
        Publishing { tx, done: done_rx }
    }
}

/// `c0mpute stream ingest`: run the hosted RTMP ingest until ctrl-c.
pub async fn serve(opts: IngestOptions) -> Result<()> {
    let api = Api::new(&opts.api, &opts.secret)?;
    let ffmpeg = crate::supervisor::which_ffmpeg().context("ffmpeg is not on PATH; the ingest needs it")?;
    let supervisor = crate::Supervisor::boot_buyer(crate::Config::default()).await?;
    let data = crate::config::data_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let keypair = c0mpute_net::identity::load_or_create(&data.join("buyer"))?;
    let owner = record::owner_hex(&keypair).context("live streams need an ed25519 identity")?;
    let live = LiveStream::start(supervisor.libp2p.clone(), supervisor.store.clone(), Timing::default()).await?;
    live.attach_tap(&supervisor.live_tap);
    let peer = supervisor.libp2p.peer_id().to_base58();

    let gate = Arc::new(Gate::new(api, owner.clone()));
    let sink = Arc::new(FfmpegSink {
        gate: gate.clone(),
        session: broadcast::Session {
            live: live.clone(),
            keypair,
            ffmpeg,
            work_root: data.join("live-ingest"),
        },
        peer: peer.clone(),
        segment_secs: opts.segment_secs.clamp(2, 10),
        window: opts.window.clamp(3, record::MAX_WINDOW),
        linger: opts.linger,
    });
    let listener = TcpListener::bind(opts.listen)
        .await
        .with_context(|| format!("bind {}", opts.listen))?;
    println!("live ingest: rtmp://{}/{APP}/<stream_key>", opts.listen);
    println!("api:         {}", opts.api.trim_end_matches('/'));
    println!("peer:        {peer}");
    println!("owner key:   {owner}   (streams from this ingest are signed with it)");

    tokio::select! {
        _ = accept_loop(listener, gate.clone(), sink) => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    // Every stream hears the same ctrl-c and ends itself; give them the
    // time to say so (end record, end event) before the process goes.
    let deadline = tokio::time::Instant::now() + opts.linger + Duration::from_secs(8);
    while gate.live() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    live.publish_receipts(meter::epoch_of(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    ))
    .await;
    live.stop();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SECRET: &str = "ingest-secret";
    const GOOD: &str = "c0l_goodkey123";
    const FREE: &str = "c0l_freekey123";

    #[derive(Default)]
    struct Mock {
        auth_calls: AtomicUsize,
        events: Mutex<Vec<serde_json::Value>>,
    }

    async fn mock_auth(
        State(m): State<Arc<Mock>>,
        headers: HeaderMap,
        axum::Json(body): axum::Json<serde_json::Value>,
    ) -> (StatusCode, axum::Json<serde_json::Value>) {
        m.auth_calls.fetch_add(1, Ordering::SeqCst);
        let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
        if bearer != format!("Bearer {SECRET}") {
            return (StatusCode::UNAUTHORIZED, axum::Json(json!({"ok": false})));
        }
        match body["stream_key"].as_str() {
            Some(GOOD) => (
                StatusCode::OK,
                axum::Json(json!({"ok": true, "id": "lv_abc", "account": "user_1", "name": "My Show",
                    "paid": true, "max_height": null, "max_viewers": null})),
            ),
            Some(FREE) => (
                StatusCode::OK,
                axum::Json(json!({"ok": true, "id": "lv_free", "account": "user_2", "name": "My Show",
                    "paid": false, "max_height": 720, "max_viewers": 25})),
            ),
            _ => (StatusCode::FORBIDDEN, axum::Json(json!({"ok": false}))),
        }
    }

    async fn mock_event(
        State(m): State<Arc<Mock>>,
        axum::Json(body): axum::Json<serde_json::Value>,
    ) -> axum::Json<serde_json::Value> {
        m.events.lock().unwrap().push(body);
        axum::Json(json!({"ok": true}))
    }

    async fn mock_api() -> (String, Arc<Mock>) {
        let mock = Arc::new(Mock::default());
        let app = axum::Router::new()
            .route("/api/v1/live/ingest/auth", post(mock_auth))
            .route("/api/v1/live/ingest/event", post(mock_event))
            .with_state(mock.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        (format!("http://{addr}"), mock)
    }

    const OWNER: &str = "aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11";

    #[tokio::test]
    async fn a_good_key_is_admitted_and_a_bad_one_refused() {
        let (base, mock) = mock_api().await;
        let gate = Gate::new(Api::new(&format!("{base}/"), SECRET).unwrap(), OWNER.into());

        let adm = gate.admit("live", GOOD).await.expect("good key");
        assert_eq!(adm.id, "lv_abc");
        assert_eq!(adm.stream_key, GOOD);
        assert_eq!(adm.name, network_name("user_1", "My Show"));
        assert!(record::valid_name(&adm.name));
        assert_eq!(adm.stream_id, record::stream_id_for(OWNER, &adm.name));
        assert!(record::valid_stream_id(&adm.stream_id));
        assert_eq!(
            adm.binding,
            Binding {
                account: "user_1".into(),
                paid: true,
                max_viewers: None
            }
        );
        assert_eq!(adm.max_height, None);
        assert_eq!(gate.live(), 1);

        // Same stream twice at once: refused; after it ends: fine again.
        assert!(gate.admit("live", GOOD).await.is_err());
        gate.release(&adm.name);
        assert!(gate.admit("live", &format!("{GOOD}?foo=1")).await.is_ok(), "a query after the key is dropped");

        // Free tier: 720p, 25 viewers, relays unpaid.
        let free = gate.admit("live", FREE).await.unwrap();
        assert_eq!(free.max_height, Some(720));
        assert_eq!(free.binding.max_viewers, Some(25));
        assert!(!free.binding.paid);
        assert_ne!(free.name, adm.name, "same stream name, another account: another network name");

        // A key c0mpute.com does not know: 403, refused.
        let calls = mock.auth_calls.load(Ordering::SeqCst);
        assert_eq!(gate.admit("live", "c0l_nosuchkey1").await.unwrap_err(), "stream key refused");
        assert_eq!(mock.auth_calls.load(Ordering::SeqCst), calls + 1);
        // Not even shaped like a key, or the wrong app: refused without a call.
        assert!(gate.admit("live", "hunter2").await.is_err());
        assert!(gate.admit("live", "").await.is_err());
        assert!(gate.admit("other", GOOD).await.is_err());
        assert_eq!(mock.auth_calls.load(Ordering::SeqCst), calls + 1);
        assert_eq!(gate.live(), 2, "refusals hold nothing");
    }

    #[tokio::test]
    async fn a_wrong_secret_or_a_dead_api_refuses_everyone() {
        let (base, _mock) = mock_api().await;
        let gate = Gate::new(Api::new(&base, "not-the-secret").unwrap(), OWNER.into());
        assert!(gate.admit("live", GOOD).await.is_err());
        let gate = Gate::new(Api::new("http://127.0.0.1:9", SECRET).unwrap(), OWNER.into());
        assert!(gate.admit("live", GOOD).await.is_err());
        assert_eq!(gate.live(), 0);
        assert!(Api::new(&base, "  ").is_err(), "no secret, no ingest");
    }

    #[tokio::test]
    async fn events_carry_the_network_stream_id_and_peer() {
        let (base, mock) = mock_api().await;
        let api = Api::new(&base, SECRET).unwrap();
        api.event("lv_abc", "start", "0123456789abcdef0123", "12D3KooWpeer").await.unwrap();
        api.event("lv_abc", "end", "0123456789abcdef0123", "12D3KooWpeer").await.unwrap();
        let events = mock.events.lock().unwrap().clone();
        assert_eq!(
            events[0],
            json!({"id": "lv_abc", "event": "start", "stream_id": "0123456789abcdef0123", "peer": "12D3KooWpeer"})
        );
        assert_eq!(events[1]["event"], "end");
    }

    #[test]
    fn auth_replies_parse() {
        let r = AuthReply::parse(&json!({"ok": true, "id": "lv_1", "account": 42, "name": "a", "paid": true,
            "max_height": null, "max_viewers": null}))
        .unwrap();
        assert_eq!(r.account, "42");
        assert!(r.paid && r.max_height.is_none());
        assert!(AuthReply::parse(&json!({"ok": false})).is_none());
        assert!(AuthReply::parse(&json!({"ok": true, "id": "lv_1"})).is_none());
        assert!(AuthReply::parse(&json!({"id": "lv_1", "account": "a", "name": "b"})).is_none());
    }

    #[test]
    fn network_names_are_valid_and_per_account() {
        let a = network_name("user_1", "My Show!");
        assert!(a.starts_with("my-show-"), "{a}");
        assert!(record::valid_name(&a));
        assert_eq!(a, network_name("user_1", "My Show!"));
        assert_ne!(a, network_name("user_2", "My Show!"));
        assert_ne!(network_name("a", "b-c"), network_name("a-b", "c"));
        let long = "x".repeat(300);
        for name in ["", "日本語", "---", long.as_str()] {
            let n = network_name("u", name);
            assert!(record::valid_name(&n), "{n}");
        }
        assert!(valid_stream_key("c0l_abcDEF123-_"));
        assert!(!valid_stream_key("c0l_"));
        assert!(!valid_stream_key("c0k_abcdef123"));
        assert!(!valid_stream_key("c0l_abc def"));
        assert!(!valid_stream_key(&format!("c0l_{}", "a".repeat(200))));
    }

    #[test]
    fn flv_framing() {
        assert_eq!(&flv_header()[..3], b"FLV");
        assert_eq!(flv_header()[4], 0x05, "audio + video");
        let tag = flv_tag(9, 0x0102_0304, b"abcde");
        assert_eq!(tag[0], 9);
        assert_eq!(&tag[1..4], &[0, 0, 5], "payload size");
        assert_eq!(&tag[4..7], &[0x02, 0x03, 0x04], "timestamp low 24 bits");
        assert_eq!(tag[7], 0x01, "timestamp high byte");
        assert_eq!(&tag[8..11], &[0, 0, 0], "stream id");
        assert_eq!(&tag[11..16], b"abcde");
        assert_eq!(&tag[16..], &[0, 0, 0, 16], "previous tag size = 11 + payload");
    }

    /// A sink that collects the FLV instead of running ffmpeg, and counts
    /// how many streams were started.
    #[derive(Default)]
    struct Collect {
        started: AtomicUsize,
        out: Mutex<Option<mpsc::UnboundedSender<(Admission, Vec<u8>)>>>,
    }

    impl Sink for Collect {
        fn start(&self, admission: Admission) -> Publishing {
            self.started.fetch_add(1, Ordering::SeqCst);
            let (tx, mut rx) = mpsc::channel::<Bytes>(64);
            let (done_tx, done_rx) = oneshot::channel::<()>();
            let out = self.out.lock().unwrap().clone();
            tokio::spawn(async move {
                let mut all = Vec::new();
                while let Some(b) = rx.recv().await {
                    all.extend_from_slice(&b);
                }
                if let Some(out) = out {
                    let _ = out.send((admission, all));
                }
                drop(done_tx);
            });
            Publishing { tx, done: done_rx }
        }
    }

    /// A minimal RTMP publisher: handshake, connect to `live`, publish `key`
    /// and, when accepted, send one video message. Returns whether the
    /// publish was accepted.
    async fn rtmp_publish(addr: SocketAddr, key: &str) -> bool {
        use rml_rtmp::sessions::{
            ClientSession, ClientSessionConfig, ClientSessionEvent, ClientSessionResult, PublishRequestType,
        };
        use rml_rtmp::time::RtmpTimestamp;
        let mut sock = TcpStream::connect(addr).await.unwrap();
        let mut hs = Handshake::new(PeerType::Client);
        sock.write_all(&hs.generate_outbound_p0_and_p1().unwrap()).await.unwrap();
        let mut buf = vec![0u8; 16 * 1024];
        let mut leftover = loop {
            let n = sock.read(&mut buf).await.unwrap();
            assert!(n > 0, "server closed during the handshake");
            match hs.process_bytes(&buf[..n]).unwrap() {
                HandshakeProcessResult::InProgress { response_bytes } => sock.write_all(&response_bytes).await.unwrap(),
                HandshakeProcessResult::Completed {
                    response_bytes,
                    remaining_bytes,
                } => {
                    sock.write_all(&response_bytes).await.unwrap();
                    break remaining_bytes;
                }
            }
        };
        let (mut cs, initial) = ClientSession::new(ClientSessionConfig::new()).unwrap();
        let mut queue: VecDeque<ClientSessionResult> = initial.into();
        queue.push_back(cs.request_connection(APP.to_string()).unwrap());
        loop {
            while let Some(r) = queue.pop_front() {
                match r {
                    ClientSessionResult::OutboundResponse(p) => {
                        if sock.write_all(&p.bytes).await.is_err() {
                            return false;
                        }
                    }
                    ClientSessionResult::RaisedEvent(ClientSessionEvent::ConnectionRequestAccepted) => {
                        queue.push_back(cs.request_publishing(key.to_string(), PublishRequestType::Live).unwrap());
                    }
                    ClientSessionResult::RaisedEvent(ClientSessionEvent::PublishRequestAccepted) => {
                        let video = cs
                            .publish_video_data(Bytes::from_static(b"\x17\x01keyframe"), RtmpTimestamp::new(40), false)
                            .unwrap();
                        if let ClientSessionResult::OutboundResponse(p) = video {
                            sock.write_all(&p.bytes).await.unwrap();
                        }
                        let _ = sock.shutdown().await;
                        return true;
                    }
                    _ => {}
                }
            }
            if !leftover.is_empty() {
                let more = cs.handle_input(&leftover).unwrap();
                leftover.clear();
                queue.extend(more);
                continue;
            }
            let n = match tokio::time::timeout(Duration::from_secs(10), sock.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => n,
                _ => return false,
            };
            match cs.handle_input(&buf[..n]) {
                Ok(more) => queue.extend(more),
                Err(_) => return false,
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rtmp_publish_with_a_bad_key_starts_nothing_and_a_good_key_streams() {
        let (base, mock) = mock_api().await;
        let gate = Arc::new(Gate::new(Api::new(&base, SECRET).unwrap(), OWNER.into()));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let sink = Arc::new(Collect {
            started: AtomicUsize::new(0),
            out: Mutex::new(Some(out_tx)),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(accept_loop(listener, gate.clone(), sink.clone()));

        // Bad key: the publish is refused and no stream (no ffmpeg) starts.
        assert!(!rtmp_publish(addr, "c0l_nosuchkey1").await);
        assert_eq!(sink.started.load(Ordering::SeqCst), 0, "a refused key must start nothing");
        assert_eq!(gate.live(), 0);
        assert_eq!(mock.auth_calls.load(Ordering::SeqCst), 1, "the key was checked with the API");

        // Good key: accepted, and the media arrives as FLV.
        assert!(rtmp_publish(addr, GOOD).await);
        let (admission, flv) = tokio::time::timeout(Duration::from_secs(10), out_rx.recv())
            .await
            .expect("the stream ended")
            .unwrap();
        assert_eq!(sink.started.load(Ordering::SeqCst), 1);
        assert_eq!(admission.id, "lv_abc");
        assert_eq!(&flv[..13], &flv_header());
        let expect = flv_tag(9, 40, b"\x17\x01keyframe");
        assert_eq!(&flv[13..], &expect[..], "one video tag, as published");
    }
}
