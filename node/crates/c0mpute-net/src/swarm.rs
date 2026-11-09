//! libp2p swarm for c0mpute.
//!
//! Phase 1 surface:
//!   - tcp + noise + yamux transport
//!   - kad DHT under `/c0mpute/kad/1.0.0` for peer + content discovery
//!   - identify + ping for housekeeping
//!   - request/response under `/c0mpute/chunk-fetch/1.0.0` for chunk bytes
//!   - request/response under `/c0mpute/rpc/1.0.0`: small named-service
//!     calls between peers (secure-chat mailboxes use it); services register
//!     an [`RpcHandler`] with [`Libp2pNetwork::register_rpc`]
//!
//! The swarm runs on a tokio task; the `Network` trait impl talks to it
//! through mpsc channels.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use c0mpute_proto::{ChunkRequest, Hash};
use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, gossipsub, identify, identity, kad, mdns, ping,
    request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tracing::{debug, info};

use crate::{ChunkSource, Network, identity as id_mod};

const KAD_PROTOCOL: StreamProtocol = StreamProtocol::new("/c0mpute/kad/1.0.0");
const FETCH_PROTOCOL: StreamProtocol = StreamProtocol::new("/c0mpute/chunk-fetch/1.0.0");
/// Same request, but the chunk travels as a CBOR byte string. Version 1 sends
/// it as an array of integers: about 1.9x the bytes on the wire and slow to
/// decode, which halved every chunk transfer. Peers that do not speak 2 are
/// asked over 1.
const FETCH2_PROTOCOL: StreamProtocol = StreamProtocol::new("/c0mpute/chunk-fetch/2.0.0");
const RPC_PROTOCOL: StreamProtocol = StreamProtocol::new("/c0mpute/rpc/1.0.0");
/// Largest RPC request body accepted.
pub const RPC_MAX_REQUEST: u64 = 1024 * 1024;
/// Largest RPC response body accepted.
pub const RPC_MAX_RESPONSE: u64 = 8 * 1024 * 1024;

/// Serves one named RPC service (see [`Libp2pNetwork::register_rpc`]).
#[async_trait]
pub trait RpcHandler: Send + Sync + 'static {
    /// Answer a request from `peer`. `Err` is sent back as an error string.
    async fn handle(&self, peer: PeerId, body: Vec<u8>) -> std::result::Result<Vec<u8>, String>;
}

type RpcHandlers = Arc<std::sync::RwLock<HashMap<String, Arc<dyn RpcHandler>>>>;

/// RPC service that hands one pubsub message straight to a peer, which
/// delivers it to its own subscribers exactly as if it had arrived on the
/// gossip mesh, attributed to the (authenticated) calling peer. Auction
/// traffic between a buyer and a bidder (bids, accepts, receipts) goes this
/// way as well as over gossipsub: the mesh drops messages while it re-forms
/// (every restart, every mDNS flap), and a lost bid is a failed auction.
pub const DIRECT_SERVICE: &str = "c0mpute.direct.v1";

/// Only job-dispatch topics may be delivered directly: everything else stays
/// gossip-only.
pub const DIRECT_TOPIC_PREFIX: &str = "c0mpute/jobs/";

fn mdns_disabled() -> bool {
    std::env::var("C0MPUTE_DISABLE_MDNS").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// `[u16 BE topic length][topic][payload]`.
fn encode_direct(topic: &str, payload: &[u8]) -> Vec<u8> {
    let t = topic.as_bytes();
    let mut out = Vec::with_capacity(2 + t.len() + payload.len());
    out.extend_from_slice(&(t.len().min(u16::MAX as usize) as u16).to_be_bytes());
    out.extend_from_slice(&t[..t.len().min(u16::MAX as usize)]);
    out.extend_from_slice(payload);
    out
}

fn decode_direct(body: &[u8]) -> Option<(String, Vec<u8>)> {
    let len = u16::from_be_bytes([*body.first()?, *body.get(1)?]) as usize;
    let topic = std::str::from_utf8(body.get(2..2 + len)?).ok()?.to_string();
    Some((topic, body[2 + len..].to_vec()))
}

/// Serves [`DIRECT_SERVICE`]: re-emits the message on the local gossip
/// stream with `source` = the peer that sent it. The peer id comes from the
/// noise handshake, so it is as authenticated as a signed gossip source.
struct DirectHandler {
    gossip_tx: broadcast::Sender<GossipMessage>,
}

#[async_trait]
impl RpcHandler for DirectHandler {
    async fn handle(&self, peer: PeerId, body: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
        let (topic, data) = decode_direct(&body).ok_or("malformed direct message")?;
        if !topic.starts_with(DIRECT_TOPIC_PREFIX) {
            return Err(format!("{topic}: not deliverable directly"));
        }
        let _ = self.gossip_tx.send(GossipMessage {
            topic,
            source: Some(peer),
            data,
        });
        Ok(Vec::new())
    }
}
const IDENTIFY_PROTOCOL: &str = "/c0mpute/id/1.0.0";

/// What the swarm task accepts from the public API.
enum Cmd {
    Listen {
        addr: Multiaddr,
        reply: oneshot::Sender<Result<()>>,
    },
    Dial {
        addr: Multiaddr,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Make sure there is a connection to `peer`, dialing it by id (Kad and
    /// identify supply its addresses).
    Connect {
        peer: PeerId,
        reply: oneshot::Sender<Result<()>>,
    },
    IsConnected {
        peer: PeerId,
        reply: oneshot::Sender<bool>,
    },
    /// Teach Kad an address for `peer` (learned from gossip), so a later
    /// dial or fetch by peer id can reach it.
    AddAddress {
        peer: PeerId,
        addr: Multiaddr,
    },
    /// Our own dialable addresses: confirmed external ones, then listeners.
    Advertised {
        reply: oneshot::Sender<Vec<Multiaddr>>,
    },
    Announce {
        hash: Hash,
        reply: oneshot::Sender<Result<()>>,
    },
    Fetch {
        req: ChunkRequest,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    FetchFrom {
        peer: PeerId,
        req: ChunkRequest,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    Addrs {
        reply: oneshot::Sender<Vec<Multiaddr>>,
    },
    Rpc {
        peer: PeerId,
        service: String,
        body: Vec<u8>,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    RpcRespond {
        channel: request_response::ResponseChannel<RpcResponse>,
        resp: RpcResponse,
    },
    FetchRespond {
        channel: request_response::ResponseChannel<FetchResponse>,
        resp: FetchResponse,
    },
    Fetch2Respond {
        channel: request_response::ResponseChannel<FetchResponse2>,
        resp: FetchResponse2,
    },
    ConnectedPeers {
        reply: oneshot::Sender<Vec<PeerId>>,
    },
    Publish {
        topic: String,
        payload: Vec<u8>,
        reply: oneshot::Sender<Result<()>>,
    },
    Subscribe {
        topic: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// DHT crawl primitive: ask Kad for peers closest to `key`, used
    /// by the status aggregator to actively walk the keyspace and
    /// surface peers that haven't recently published on subscribed
    /// gossipsub topics. Bitmagnet-style.
    KadFindNode {
        key: Vec<u8>,
        reply: oneshot::Sender<()>,
    },
    /// Look `peer` up in the DHT and learn its addresses.
    FindPeer {
        peer: PeerId,
        reply: oneshot::Sender<Vec<Multiaddr>>,
    },
}

/// A gossipsub message delivered to a subscribed topic.
#[derive(Clone, Debug)]
pub struct GossipMessage {
    pub topic: String,
    pub source: Option<PeerId>,
    pub data: Vec<u8>,
}

/// Public configuration knobs.
#[derive(Clone)]
pub struct NetworkConfig {
    /// Directory holding the persistent identity key.
    pub identity_dir: PathBuf,
    /// Bootstrap peers to dial on startup.
    pub bootstrap: Vec<Multiaddr>,
    /// libp2p listen addresses. Default: `/ip4/0.0.0.0/tcp/0` (random port).
    pub listen: Vec<Multiaddr>,
    /// Optional local chunk source — when peers request a chunk we hold,
    /// we serve it from here. None = serve nothing (read-only client).
    pub local_source: Option<Arc<dyn ChunkSource>>,
}

impl NetworkConfig {
    pub fn for_dir(identity_dir: impl Into<PathBuf>) -> Self {
        Self {
            identity_dir: identity_dir.into(),
            bootstrap: vec![],
            listen: vec!["/ip4/0.0.0.0/tcp/0".parse().unwrap()],
            local_source: None,
        }
    }

    pub fn with_bootstrap(mut self, addrs: Vec<Multiaddr>) -> Self {
        self.bootstrap = addrs;
        self
    }

    pub fn with_listen(mut self, addrs: Vec<Multiaddr>) -> Self {
        self.listen = addrs;
        self
    }

    pub fn with_local_source(mut self, src: Arc<dyn ChunkSource>) -> Self {
        self.local_source = Some(src);
        self
    }
}

/// `Network` trait impl backed by a real libp2p Swarm.
pub struct Libp2pNetwork {
    cmd_tx: mpsc::Sender<Cmd>,
    gossip_tx: broadcast::Sender<GossipMessage>,
    peer_id: PeerId,
    rpc_handlers: RpcHandlers,
    /// The node identity, kept to sign statements other peers check against
    /// our peer id ([`Self::sign`]).
    keypair: identity::Keypair,
    /// Root of the per-job keys this node derives as a buyer
    /// ([`Self::derive_job_secret`]). Never leaves this struct.
    job_secret: Arc<zeroize::Zeroizing<[u8; 32]>>,
    /// This swarm's random instance id ([`Self::instance_id`]).
    instance_id: String,
}

/// A fresh instance id: zero-padded unix-ms start time, `-`, 8 random hex
/// digits. Ordering two ids as strings orders them by start time first, so
/// "the newer process" is simply the larger id.
pub fn new_instance_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    format!("{ms:013}-{:08x}", rand::random::<u32>())
}

/// The start time (unix ms) an instance id from [`new_instance_id`] carries.
pub fn instance_started_ms(id: &str) -> Option<u64> {
    id.split('-').next()?.parse().ok()
}

impl Libp2pNetwork {
    /// Spawn the swarm task and return a handle. The task runs until the
    /// returned `Libp2pNetwork` is dropped.
    pub async fn spawn(config: NetworkConfig) -> Result<Self> {
        let keypair = id_mod::load_or_create(&config.identity_dir)?;
        let job_secret = Arc::new(id_mod::load_or_create_job_secret(&config.identity_dir)?);
        let peer_id = keypair.public().to_peer_id();

        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(64);
        let (gossip_tx, _) = broadcast::channel::<GossipMessage>(256);

        let rpc_handlers: RpcHandlers = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let task = SwarmTask::new(
            keypair.clone(),
            config.local_source,
            gossip_tx.clone(),
            rpc_handlers.clone(),
            cmd_tx.downgrade(),
        )
        .await?;
        for addr in &config.listen {
            task.swarm
                .lock()
                .await
                .listen_on(addr.clone())
                .with_context(|| format!("listen on {addr}"))?;
        }
        for addr in &config.bootstrap {
            task.swarm
                .lock()
                .await
                .dial(addr.clone())
                .with_context(|| format!("dial bootstrap {addr}"))?;
        }

        tokio::spawn(task.run(cmd_rx));

        if let Ok(mut h) = rpc_handlers.write() {
            h.insert(
                DIRECT_SERVICE.to_string(),
                Arc::new(DirectHandler {
                    gossip_tx: gossip_tx.clone(),
                }),
            );
        }

        Ok(Self {
            cmd_tx,
            gossip_tx,
            peer_id,
            rpc_handlers,
            keypair,
            job_secret,
            instance_id: new_instance_id(),
        })
    }

    /// A random id for this running swarm, new on every start. The peer id
    /// says who a node is; the instance id says which process: two with the
    /// same peer id and different instance ids are twins (one identity
    /// running twice), which buyers and the node board detect from bids,
    /// accepts and capability ads.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// A 32-byte secret for `context`, derived from this node's job-key
    /// secret with HKDF-SHA256 (domain `c0mpute/job-secret/v1`). The same
    /// node and context always give the same bytes, across restarts; nothing
    /// about the root secret can be learned from them.
    pub fn derive_job_secret(&self, context: &[u8]) -> zeroize::Zeroizing<[u8; 32]> {
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(b"c0mpute/job-secret/v1"), &**self.job_secret);
        let mut out = zeroize::Zeroizing::new([0u8; 32]);
        hk.expand(context, &mut *out).expect("32-byte okm");
        out
    }

    /// Sign `msg` with this node's identity key. Anyone can check it against
    /// our peer id with [`crate::identity::verify_peer_signature`].
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>> {
        self.keypair.sign(msg).map_err(|e| anyhow!("sign: {e}"))
    }

    /// Serve `service` over `/c0mpute/rpc/1.0.0`. Replaces any handler
    /// already registered under the name.
    pub fn register_rpc(&self, service: &str, handler: Arc<dyn RpcHandler>) {
        if let Ok(mut h) = self.rpc_handlers.write() {
            h.insert(service.to_string(), handler);
        }
    }

    /// Call `service` on `peer` (dialing it if needed) and wait for the
    /// answer. The request times out after 20 s.
    pub async fn rpc(&self, peer: PeerId, service: &str, body: Vec<u8>) -> Result<Vec<u8>> {
        if peer == self.peer_id {
            let handler = self.rpc_handlers.read().ok().and_then(|h| h.get(service).cloned());
            return match handler {
                Some(h) => h.handle(peer, body).await.map_err(|e| anyhow!("{e}")),
                None => Err(anyhow!("{service}: not served here")),
            };
        }
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Rpc {
                peer,
                service: service.to_string(),
                body,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("swarm task closed"))?;
        rx.await.map_err(|_| anyhow!("swarm task closed"))?
    }

    /// Deliver `payload` on `topic` straight to `peer` ([`DIRECT_SERVICE`]),
    /// dialing it if needed, and wait up to `timeout` for it to confirm.
    /// `peer` hands it to its local subscribers as if it came over gossip
    /// from us. Only `c0mpute/jobs/*` topics; never to ourselves (gossipsub
    /// does not deliver our own messages to us either).
    pub async fn send_direct(
        &self,
        peer: PeerId,
        topic: &str,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<()> {
        if peer == self.peer_id {
            return Err(anyhow!("direct send to ourselves"));
        }
        if !topic.starts_with(DIRECT_TOPIC_PREFIX) {
            return Err(anyhow!("{topic}: not deliverable directly"));
        }
        let body = encode_direct(topic, payload);
        tokio::time::timeout(timeout, self.rpc(peer, DIRECT_SERVICE, body))
            .await
            .map_err(|_| anyhow!("direct send to {peer} timed out after {timeout:?}"))??;
        Ok(())
    }

    /// Peers with an open connection now.
    pub async fn connected_peers(&self) -> Vec<PeerId> {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(Cmd::ConnectedPeers { reply: tx }).await.is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Publish bytes to a gossipsub topic. The topic must have been
    /// `subscribe`d first (libp2p subscribes-to-publish to participate
    /// in the mesh).
    pub async fn publish(&self, topic: &str, payload: Vec<u8>) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Publish {
                topic: topic.to_string(),
                payload,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("swarm task closed"))?;
        rx.await.map_err(|_| anyhow!("swarm task closed"))?
    }

    /// Subscribe to a gossipsub topic. Use `messages()` to receive
    /// delivered payloads on the broadcast stream.
    pub async fn subscribe(&self, topic: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Subscribe {
                topic: topic.to_string(),
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("swarm task closed"))?;
        rx.await.map_err(|_| anyhow!("swarm task closed"))?
    }

    /// Receive gossipsub messages delivered to any subscribed topic.
    /// Messages predating the call are not replayed; this is a live
    /// stream from the moment of subscription.
    pub fn messages(&self) -> broadcast::Receiver<GossipMessage> {
        self.gossip_tx.subscribe()
    }

    /// Kick the Kad routing table by querying for peers close to a
    /// random key. Used by the status aggregator's DHT crawler to
    /// surface peers we haven't heard from on gossipsub. The query
    /// runs asynchronously inside the swarm; results show up in the
    /// routing table for subsequent lookups.
    pub async fn kad_find_node(&self, key: Vec<u8>) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::KadFindNode { key, reply: tx })
            .await
            .map_err(|_| anyhow!("swarm task closed"))?;
        rx.await.map_err(|_| anyhow!("swarm task closed"))?;
        Ok(())
    }

    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Ask the DHT for `peer`'s addresses (a Kademlia lookup of its id) and
    /// teach them to the routing table, so a dial by peer id works for a peer
    /// we only ever heard of through gossip relayed by others. Returns the
    /// addresses found (empty when the lookup found none within `timeout`).
    pub async fn find_peer(&self, peer: PeerId, timeout: Duration) -> Vec<Multiaddr> {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(Cmd::FindPeer { peer, reply: tx }).await.is_err() {
            return Vec::new();
        }
        tokio::time::timeout(timeout, rx).await.ok().and_then(|r| r.ok()).unwrap_or_default()
    }

    /// Make sure there is a connection to `peer`: already connected, or a
    /// dial with the addresses we know, or a DHT lookup for its addresses and
    /// a dial with those. Used before point-to-point exchanges with a peer
    /// heard of only through gossip.
    pub async fn reach(&self, peer: PeerId, timeout: Duration) -> Result<()> {
        if peer == self.peer_id || self.is_connected(peer).await {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let wait_connected = |deadline: tokio::time::Instant| async move {
            while tokio::time::Instant::now() < deadline {
                if self.is_connected(peer).await {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            false
        };
        // Dial with what we know; failing that, ask the DHT (a routing table
        // still filling right after start may need another round) and dial
        // again, until the deadline.
        loop {
            let dialed = self.connect(peer).await;
            debug!(%peer, dial = ?dialed.as_ref().err(), "reach: dial by peer id");
            let step = (tokio::time::Instant::now() + Duration::from_millis(1500)).min(deadline);
            if dialed.is_ok() && wait_connected(step).await {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                bail!("could not reach {peer} within {timeout:?}");
            }
            if self.find_peer(peer, left.min(Duration::from_secs(3))).await.is_empty() {
                tokio::time::sleep(Duration::from_millis(500).min(left)).await;
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("could not reach {peer} within {timeout:?}");
            }
        }
    }

    /// Fetch a chunk directly from a peer already identified by a job offer
    /// or receipt. This avoids waiting for a DHT provider record to propagate.
    pub async fn fetch_from_peer(&self, peer: PeerId, req: &ChunkRequest) -> Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::FetchFrom {
                peer,
                req: req.clone(),
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("swarm task closed"))?;
        rx.await.map_err(|_| anyhow!("swarm task closed"))?
    }

    pub async fn listen_addrs(&self) -> Result<Vec<Multiaddr>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Addrs { reply: tx })
            .await
            .map_err(|_| anyhow!("swarm task closed"))?;
        rx.await.map_err(|_| anyhow!("swarm task closed"))
    }

    pub async fn listen_on(&self, addr: Multiaddr) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Listen { addr, reply: tx })
            .await
            .map_err(|_| anyhow!("swarm task closed"))?;
        rx.await.map_err(|_| anyhow!("swarm task closed"))?
    }

    /// Open (or reuse) a connection to `peer` by id. A worker uses this to
    /// reach its buyer before publishing a result: a worker behind NAT can
    /// dial out, and the buyer's result fetch then rides that connection.
    /// Returns once the dial is issued.
    pub async fn connect(&self, peer: PeerId) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Connect { peer, reply: tx })
            .await
            .map_err(|_| anyhow!("swarm task closed"))?;
        rx.await.map_err(|_| anyhow!("swarm task closed"))?
    }

    /// Teach the routing table an address for `peer`.
    pub async fn add_peer_address(&self, peer: PeerId, addr: Multiaddr) {
        let _ = self.cmd_tx.send(Cmd::AddAddress { peer, addr }).await;
    }

    /// This node's dialable addresses (external first, then listeners).
    pub async fn advertised_addrs(&self) -> Vec<Multiaddr> {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(Cmd::Advertised { reply: tx }).await.is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Whether a connection to `peer` is open now.
    pub async fn is_connected(&self, peer: PeerId) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(Cmd::IsConnected { peer, reply: tx })
            .await
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub async fn dial(&self, addr: Multiaddr) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Dial { addr, reply: tx })
            .await
            .map_err(|_| anyhow!("swarm task closed"))?;
        rx.await.map_err(|_| anyhow!("swarm task closed"))?
    }
}

#[async_trait]
impl Network for Libp2pNetwork {
    async fn announce(&self, hash: &Hash) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Announce {
                hash: *hash,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("swarm task closed"))?;
        rx.await.map_err(|_| anyhow!("swarm task closed"))?
    }

    async fn fetch(&self, req: &ChunkRequest) -> Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Fetch {
                req: req.clone(),
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("swarm task closed"))?;
        rx.await.map_err(|_| anyhow!("swarm task closed"))?
    }
}

// ────────────────────────────────────────────────────────────────────────
// Behaviour
// ────────────────────────────────────────────────────────────────────────

#[derive(NetworkBehaviour)]
struct C0mputeBehaviour {
    kad: kad::Behaviour<kad::store::MemoryStore>,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    fetch: request_response::cbor::Behaviour<FetchRequest, FetchResponse>,
    fetch2: request_response::cbor::Behaviour<FetchRequest, FetchResponse2>,
    rpc: request_response::Behaviour<request_response::cbor::codec::Codec<RpcRequest, RpcResponse>>,
    gossipsub: gossipsub::Behaviour,
    mdns: libp2p::swarm::behaviour::toggle::Toggle<mdns::tokio::Behaviour>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct FetchRequest {
    chunk_hash: Hash,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
enum FetchResponse {
    Ok { bytes: Vec<u8> },
    NotFound,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
enum FetchResponse2 {
    Ok {
        #[serde(with = "cbor_bytes")]
        bytes: Vec<u8>,
    },
    NotFound,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct RpcRequest {
    service: String,
    #[serde(with = "cbor_bytes")]
    body: Vec<u8>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
enum RpcResponse {
    Ok {
        #[serde(with = "cbor_bytes")]
        body: Vec<u8>,
    },
    Err {
        msg: String,
    },
}

/// Encode `Vec<u8>` as a CBOR byte string (serde's default is an array of
/// integers, up to twice the size).
mod cbor_bytes {
    use serde::{Deserializer, Serializer, de};

    pub fn serialize<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(b)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        struct V;
        impl<'de> de::Visitor<'de> for V {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("bytes")
            }
            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                Ok(v.to_vec())
            }
            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(v)
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
                let mut out = Vec::new();
                while let Some(b) = seq.next_element::<u8>()? {
                    out.push(b);
                }
                Ok(out)
            }
        }
        d.deserialize_byte_buf(V)
    }
}

// ────────────────────────────────────────────────────────────────────────
// Swarm task
// ────────────────────────────────────────────────────────────────────────

type Swarm = libp2p::Swarm<C0mputeBehaviour>;

struct SwarmTask {
    swarm: Arc<Mutex<Swarm>>,
    local_source: Option<Arc<dyn ChunkSource>>,
    /// Pending `fetch` calls awaiting Kad provider lookups.
    pending_kad_lookups: HashMap<kad::QueryId, oneshot::Sender<Result<Vec<u8>>>>,
    /// Pending request-response calls.
    pending_fetches: HashMap<request_response::OutboundRequestId, oneshot::Sender<Result<Vec<u8>>>>,
    /// Track which chunk a fetch is for, so we can re-issue against another
    /// provider if the first one says NotFound.
    fetch_targets: HashMap<request_response::OutboundRequestId, Hash>,
    /// Version-2 fetches in flight: who was asked, so a peer that does not
    /// speak version 2 can be asked again over version 1.
    pending_fetches2: HashMap<request_response::OutboundRequestId, (PeerId, Hash, oneshot::Sender<Result<Vec<u8>>>)>,
    /// Peers that answered "unsupported protocol" to version 2.
    fetch_v1_only: std::collections::HashSet<PeerId>,
    /// Pending outbound RPC calls.
    pending_rpcs: HashMap<request_response::OutboundRequestId, oneshot::Sender<Result<Vec<u8>>>>,
    /// Pending `find_peer` lookups: the peer sought and who to tell.
    pending_find_peers: HashMap<kad::QueryId, (PeerId, oneshot::Sender<Vec<Multiaddr>>)>,
    rpc_handlers: RpcHandlers,
    /// For handler tasks to hand their answer back to the swarm loop (weak,
    /// so the loop still ends when the last `Libp2pNetwork` drops).
    cmd_weak: mpsc::WeakSender<Cmd>,
    /// Broadcast channel for gossipsub messages delivered to subscribed
    /// topics. Senders are upstream consumers that called `messages()`.
    gossip_tx: broadcast::Sender<GossipMessage>,
}

impl SwarmTask {
    async fn new(
        keypair: identity::Keypair,
        local_source: Option<Arc<dyn ChunkSource>>,
        gossip_tx: broadcast::Sender<GossipMessage>,
        rpc_handlers: RpcHandlers,
        cmd_weak: mpsc::WeakSender<Cmd>,
    ) -> Result<Self> {
        let swarm = libp2p::SwarmBuilder::with_existing_identity(keypair.clone())
            .with_tokio()
            .with_tcp(
                libp2p::tcp::Config::default(),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )?
            .with_behaviour(|key| {
                let local_peer_id = key.public().to_peer_id();
                let store = kad::store::MemoryStore::new(local_peer_id);
                let mut kad_cfg = kad::Config::new(KAD_PROTOCOL);
                kad_cfg.set_query_timeout(Duration::from_secs(60));
                let mut kad = kad::Behaviour::with_config(local_peer_id, store, kad_cfg);
                kad.set_mode(Some(kad::Mode::Server));

                let identify = identify::Behaviour::new(identify::Config::new(
                    IDENTIFY_PROTOCOL.into(),
                    key.public(),
                ));
                let ping = ping::Behaviour::default();
                let fetch = request_response::cbor::Behaviour::<FetchRequest, FetchResponse>::new(
                    [(FETCH_PROTOCOL, request_response::ProtocolSupport::Full)],
                    request_response::Config::default(),
                );
                let fetch2 = request_response::cbor::Behaviour::<FetchRequest, FetchResponse2>::new(
                    [(FETCH2_PROTOCOL, request_response::ProtocolSupport::Full)],
                    request_response::Config::default(),
                );
                let rpc = request_response::Behaviour::with_codec(
                    request_response::cbor::codec::Codec::<RpcRequest, RpcResponse>::default()
                        .set_request_size_maximum(RPC_MAX_REQUEST)
                        .set_response_size_maximum(RPC_MAX_RESPONSE),
                    [(RPC_PROTOCOL, request_response::ProtocolSupport::Full)],
                    request_response::Config::default().with_request_timeout(Duration::from_secs(20)),
                );
                let gossipsub_cfg = gossipsub::ConfigBuilder::default()
                    .heartbeat_interval(Duration::from_secs(1))
                    // Publish to every connected peer subscribed to the
                    // topic, not only the mesh: a bid must reach the buyer
                    // even while the mesh is still forming after a restart.
                    .flood_publish(true)
                    .validation_mode(gossipsub::ValidationMode::Strict)
                    .build()
                    .map_err(|e| std::io::Error::other(format!("gossipsub config: {e}")))?;
                let gossipsub = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Signed(key.clone()),
                    gossipsub_cfg,
                )
                .map_err(|e| std::io::Error::other(format!("gossipsub behaviour: {e}")))?;
                // `C0MPUTE_DISABLE_MDNS=1` keeps a node off LAN discovery:
                // tests run on a box beside live workers set it, or their
                // offers reach (and are won by) the real network.
                let mdns = if mdns_disabled() {
                    None
                } else {
                    Some(mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?)
                };
                let mdns = libp2p::swarm::behaviour::toggle::Toggle::from(mdns);
                Ok(C0mputeBehaviour {
                    kad,
                    identify,
                    ping,
                    fetch,
                    fetch2,
                    rpc,
                    gossipsub,
                    mdns,
                })
            })
            .map_err(|e| anyhow!("build behaviour: {e}"))?
            .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        Ok(Self {
            swarm: Arc::new(Mutex::new(swarm)),
            local_source,
            pending_kad_lookups: HashMap::new(),
            pending_fetches: HashMap::new(),
            fetch_targets: HashMap::new(),
            pending_fetches2: HashMap::new(),
            fetch_v1_only: std::collections::HashSet::new(),
            pending_rpcs: HashMap::new(),
            pending_find_peers: HashMap::new(),
            rpc_handlers,
            cmd_weak,
            gossip_tx,
        })
    }

    async fn run(mut self, mut cmd_rx: mpsc::Receiver<Cmd>) {
        info!("c0mpute-net swarm task running");
        loop {
            // Drive both: incoming swarm events AND command-channel input.
            // Holding the lock briefly for next_event then yielding lets
            // other tasks call into the channel.
            let event_or_cmd = {
                let mut swarm = self.swarm.lock().await;
                tokio::select! {
                    event = swarm.select_next_some() => Some(EventOrCmd::Event(event)),
                    maybe_cmd = cmd_rx.recv() => {
                        match maybe_cmd {
                            Some(c) => Some(EventOrCmd::Cmd(c)),
                            None => None,
                        }
                    }
                }
            };
            match event_or_cmd {
                Some(EventOrCmd::Event(e)) => self.handle_event(e).await,
                Some(EventOrCmd::Cmd(c)) => self.handle_cmd(c).await,
                None => {
                    info!("swarm command channel closed; exiting");
                    return;
                }
            }
        }
    }

    async fn handle_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Listen { addr, reply } => {
                let r = self
                    .swarm
                    .lock()
                    .await
                    .listen_on(addr)
                    .map(|_| ())
                    .map_err(Into::into);
                let _ = reply.send(r);
            }
            Cmd::Dial { addr, reply } => {
                let r = self.swarm.lock().await.dial(addr).map_err(Into::into);
                let _ = reply.send(r);
            }
            Cmd::AddAddress { peer, addr } => {
                self.swarm.lock().await.behaviour_mut().kad.add_address(&peer, addr);
            }
            Cmd::Advertised { reply } => {
                let swarm = self.swarm.lock().await;
                let mut addrs: Vec<Multiaddr> = swarm.external_addresses().cloned().collect();
                for a in swarm.listeners() {
                    if !addrs.contains(a) {
                        addrs.push(a.clone());
                    }
                }
                let _ = reply.send(addrs);
            }
            Cmd::IsConnected { peer, reply } => {
                let _ = reply.send(self.swarm.lock().await.is_connected(&peer));
            }
            Cmd::Connect { peer, reply } => {
                let mut swarm = self.swarm.lock().await;
                let r = if swarm.is_connected(&peer) {
                    Ok(())
                } else {
                    swarm.dial(peer).map_err(Into::into)
                };
                let _ = reply.send(r);
            }
            Cmd::Addrs { reply } => {
                let addrs: Vec<Multiaddr> = self.swarm.lock().await.listeners().cloned().collect();
                let _ = reply.send(addrs);
            }
            Cmd::Announce { hash, reply } => {
                // Tell the DHT we provide this chunk.
                let key = kad::RecordKey::new(&hash.0);
                let r = self
                    .swarm
                    .lock()
                    .await
                    .behaviour_mut()
                    .kad
                    .start_providing(key)
                    .map(|_| ())
                    .map_err(|e| anyhow!("start_providing: {e}"));
                let _ = reply.send(r);
            }
            Cmd::Fetch { req, reply } => {
                let key = kad::RecordKey::new(&req.chunk_hash.0);
                let qid = self
                    .swarm
                    .lock()
                    .await
                    .behaviour_mut()
                    .kad
                    .get_providers(key);
                self.pending_kad_lookups.insert(qid, reply);
                debug!(chunk = %req.chunk_hash, "kad get_providers issued");
            }
            Cmd::FetchFrom { peer, req, reply } if !self.fetch_v1_only.contains(&peer) => {
                let request_id = self.swarm.lock().await.behaviour_mut().fetch2.send_request(
                    &peer,
                    FetchRequest {
                        chunk_hash: req.chunk_hash,
                    },
                );
                self.pending_fetches2.insert(request_id, (peer, req.chunk_hash, reply));
                debug!(%peer, chunk = %req.chunk_hash, "direct chunk fetch issued (v2)");
            }
            Cmd::FetchFrom { peer, req, reply } => {
                let request_id = self.swarm.lock().await.behaviour_mut().fetch.send_request(
                    &peer,
                    FetchRequest {
                        chunk_hash: req.chunk_hash,
                    },
                );
                self.pending_fetches.insert(request_id, reply);
                self.fetch_targets.insert(request_id, req.chunk_hash);
                debug!(%peer, chunk = %req.chunk_hash, "direct chunk fetch issued");
            }
            Cmd::Rpc {
                peer,
                service,
                body,
                reply,
            } => {
                let id = self
                    .swarm
                    .lock()
                    .await
                    .behaviour_mut()
                    .rpc
                    .send_request(&peer, RpcRequest { service, body });
                self.pending_rpcs.insert(id, reply);
            }
            Cmd::RpcRespond { channel, resp } => {
                let _ = self.swarm.lock().await.behaviour_mut().rpc.send_response(channel, resp);
            }
            Cmd::FetchRespond { channel, resp } => {
                let _ = self.swarm.lock().await.behaviour_mut().fetch.send_response(channel, resp);
            }
            Cmd::Fetch2Respond { channel, resp } => {
                let _ = self.swarm.lock().await.behaviour_mut().fetch2.send_response(channel, resp);
            }
            Cmd::ConnectedPeers { reply } => {
                let peers = self.swarm.lock().await.connected_peers().copied().collect();
                let _ = reply.send(peers);
            }
            Cmd::Publish {
                topic,
                payload,
                reply,
            } => {
                let t = gossipsub::IdentTopic::new(&topic);
                let r = self
                    .swarm
                    .lock()
                    .await
                    .behaviour_mut()
                    .gossipsub
                    .publish(t, payload)
                    .map(|_| ())
                    .map_err(|e| anyhow!("gossipsub publish: {e}"));
                let _ = reply.send(r);
            }
            Cmd::Subscribe { topic, reply } => {
                let t = gossipsub::IdentTopic::new(&topic);
                let r = self
                    .swarm
                    .lock()
                    .await
                    .behaviour_mut()
                    .gossipsub
                    .subscribe(&t)
                    .map(|_| ())
                    .map_err(|e| anyhow!("gossipsub subscribe: {e}"));
                let _ = reply.send(r);
            }
            Cmd::FindPeer { peer, reply } => {
                let id = self.swarm.lock().await.behaviour_mut().kad.get_closest_peers(peer);
                self.pending_find_peers.insert(id, (peer, reply));
            }
            Cmd::KadFindNode { key, reply } => {
                let _ = self
                    .swarm
                    .lock()
                    .await
                    .behaviour_mut()
                    .kad
                    .get_closest_peers(key);
                let _ = reply.send(());
            }
        }
    }

    async fn handle_event(&mut self, event: SwarmEvent<C0mputeBehaviourEvent>) {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                info!(%address, "listening");
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                debug!(%peer_id, "connection established");
            }
            SwarmEvent::Behaviour(C0mputeBehaviourEvent::Identify(identify::Event::Received {
                peer_id,
                info,
                ..
            })) => {
                // Add the peer's listen addrs to Kad so it can be routed to.
                let mut sw = self.swarm.lock().await;
                for addr in info.listen_addrs {
                    sw.behaviour_mut().kad.add_address(&peer_id, addr);
                }
            }
            SwarmEvent::Behaviour(C0mputeBehaviourEvent::Kad(
                kad::Event::OutboundQueryProgressed { id, result, .. },
            )) => {
                self.handle_kad_progress(id, result).await;
            }
            SwarmEvent::Behaviour(C0mputeBehaviourEvent::Fetch(
                request_response::Event::Message { peer, message, .. },
            )) => {
                self.handle_fetch_message(peer, message).await;
            }
            SwarmEvent::Behaviour(C0mputeBehaviourEvent::Fetch(
                request_response::Event::OutboundFailure {
                    request_id, error, ..
                },
            )) => {
                if let Some(reply) = self.pending_fetches.remove(&request_id) {
                    self.fetch_targets.remove(&request_id);
                    let _ = reply.send(Err(anyhow!("fetch failed: {error}")));
                }
            }
            SwarmEvent::Behaviour(C0mputeBehaviourEvent::Fetch2(request_response::Event::Message {
                message, ..
            })) => match message {
                request_response::Message::Request { request, channel, .. } => {
                    // Read off the event loop: a disk read per chunk inline
                    // here stalled every other peer's traffic behind it.
                    let src = self.local_source.clone();
                    let weak = self.cmd_weak.clone();
                    tokio::spawn(async move {
                        let resp = match src {
                            Some(src) => match src.read_chunk(&request.chunk_hash).await {
                                Ok(bytes) => FetchResponse2::Ok { bytes },
                                Err(_) => FetchResponse2::NotFound,
                            },
                            None => FetchResponse2::NotFound,
                        };
                        if let Some(tx) = weak.upgrade() {
                            let _ = tx.send(Cmd::Fetch2Respond { channel, resp }).await;
                        }
                    });
                }
                request_response::Message::Response {
                    request_id, response, ..
                } => {
                    if let Some((_, _, reply)) = self.pending_fetches2.remove(&request_id) {
                        let _ = reply.send(match response {
                            FetchResponse2::Ok { bytes } => Ok(bytes),
                            FetchResponse2::NotFound => Err(anyhow!("provider returned NotFound for chunk")),
                        });
                    }
                }
            },
            SwarmEvent::Behaviour(C0mputeBehaviourEvent::Fetch2(request_response::Event::OutboundFailure {
                request_id,
                error,
                ..
            })) => {
                if let Some((peer, hash, reply)) = self.pending_fetches2.remove(&request_id) {
                    if matches!(error, request_response::OutboundFailure::UnsupportedProtocols) {
                        // An older peer: ask again over version 1, and
                        // remember not to try version 2 with it.
                        self.fetch_v1_only.insert(peer);
                        let id = self
                            .swarm
                            .lock()
                            .await
                            .behaviour_mut()
                            .fetch
                            .send_request(&peer, FetchRequest { chunk_hash: hash });
                        self.pending_fetches.insert(id, reply);
                        self.fetch_targets.insert(id, hash);
                    } else {
                        let _ = reply.send(Err(anyhow!("fetch failed: {error}")));
                    }
                }
            }
            SwarmEvent::Behaviour(C0mputeBehaviourEvent::Rpc(request_response::Event::Message {
                peer, message, ..
            })) => match message {
                request_response::Message::Request { request, channel, .. } => {
                    let handler = self
                        .rpc_handlers
                        .read()
                        .ok()
                        .and_then(|h| h.get(&request.service).cloned());
                    match handler {
                        Some(h) => {
                            let weak = self.cmd_weak.clone();
                            tokio::spawn(async move {
                                let resp = match h.handle(peer, request.body).await {
                                    Ok(body) => RpcResponse::Ok { body },
                                    Err(msg) => RpcResponse::Err { msg },
                                };
                                if let Some(tx) = weak.upgrade() {
                                    let _ = tx.send(Cmd::RpcRespond { channel, resp }).await;
                                }
                            });
                        }
                        None => {
                            let resp = RpcResponse::Err {
                                msg: format!("{}: not served here", request.service),
                            };
                            let _ = self.swarm.lock().await.behaviour_mut().rpc.send_response(channel, resp);
                        }
                    }
                }
                request_response::Message::Response {
                    request_id, response, ..
                } => {
                    if let Some(reply) = self.pending_rpcs.remove(&request_id) {
                        let _ = reply.send(match response {
                            RpcResponse::Ok { body } => Ok(body),
                            RpcResponse::Err { msg } => Err(anyhow!("{msg}")),
                        });
                    }
                }
            },
            SwarmEvent::Behaviour(C0mputeBehaviourEvent::Rpc(request_response::Event::OutboundFailure {
                request_id,
                error,
                ..
            })) => {
                if let Some(reply) = self.pending_rpcs.remove(&request_id) {
                    let _ = reply.send(Err(anyhow!("rpc failed: {error}")));
                }
            }
            SwarmEvent::Behaviour(C0mputeBehaviourEvent::Gossipsub(
                gossipsub::Event::Message {
                    propagation_source,
                    message,
                    ..
                },
            )) => {
                let msg = GossipMessage {
                    topic: message.topic.into_string(),
                    source: message.source.or(Some(propagation_source)),
                    data: message.data,
                };
                // Send to upstream consumers; ignore errors (no
                // subscribers is fine — the buffer is bounded so this
                // never blocks).
                let _ = self.gossip_tx.send(msg);
            }
            SwarmEvent::Behaviour(C0mputeBehaviourEvent::Mdns(mdns::Event::Discovered(peers))) => {
                let mut sw = self.swarm.lock().await;
                for (peer_id, addr) in peers {
                    debug!(%peer_id, %addr, "mDNS discovered peer");
                    sw.behaviour_mut().kad.add_address(&peer_id, addr.clone());
                    // A LAN neighbour: gossipsub forwards every message on a
                    // shared topic to explicit peers, mesh or not.
                    sw.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                }
            }
            // An mDNS record expiring does not mean the peer left: docker
            // bridge peers expire and are rediscovered every few seconds
            // (2,194 discoveries and 1,530 expiries in 20 minutes on dev2's
            // seed, 2026-11-09). Dropping explicit status on every expiry
            // left the peer neither explicit nor meshed (gossipsub ignores
            // GRAFTs from explicit peers) until the next discovery, and
            // messages forwarded meanwhile were lost. The peer stays explicit
            // until its last connection actually closes.
            SwarmEvent::Behaviour(C0mputeBehaviourEvent::Mdns(mdns::Event::Expired(peers))) => {
                for (peer_id, _addr) in peers {
                    debug!(%peer_id, "mDNS peer expired");
                }
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                num_established: 0,
                ..
            } => {
                self.swarm
                    .lock()
                    .await
                    .behaviour_mut()
                    .gossipsub
                    .remove_explicit_peer(&peer_id);
            }
            _ => {}
        }
    }

    async fn handle_kad_progress(&mut self, id: kad::QueryId, result: kad::QueryResult) {
        if let kad::QueryResult::GetClosestPeers(r) = &result
            && let Some((target, reply)) = self.pending_find_peers.remove(&id)
        {
            let peers = match r {
                Ok(ok) => &ok.peers,
                Err(kad::GetClosestPeersError::Timeout { peers, .. }) => peers,
            };
            let addrs: Vec<Multiaddr> = peers
                .iter()
                .filter(|p| p.peer_id == target)
                .flat_map(|p| p.addrs.iter().cloned())
                .collect();
            let mut sw = self.swarm.lock().await;
            for a in &addrs {
                sw.behaviour_mut().kad.add_address(&target, a.clone());
            }
            let table: usize = sw.behaviour_mut().kad.kbuckets().map(|b| b.num_entries()).sum();
            debug!(%target, found = addrs.len(), peers_seen = peers.len(), routing_table = table, ok = r.is_ok(), "find_peer: DHT lookup done");
            let _ = reply.send(addrs);
            return;
        }
        match result {
            kad::QueryResult::GetProviders(Ok(kad::GetProvidersOk::FoundProviders {
                key,
                providers,
            })) => {
                let Some(reply) = self.pending_kad_lookups.remove(&id) else {
                    return;
                };
                let chunk_hash = match Hash::from_hex(&hex::encode(key.as_ref())) {
                    Ok(h) => h,
                    Err(_) => {
                        let _ = reply.send(Err(anyhow!("kad returned key wasn't a valid hash")));
                        return;
                    }
                };
                if providers.is_empty() {
                    let _ = reply.send(Err(anyhow!("no providers for {}", chunk_hash)));
                    return;
                }
                let provider = *providers.iter().next().unwrap();
                let req_id = self
                    .swarm
                    .lock()
                    .await
                    .behaviour_mut()
                    .fetch
                    .send_request(&provider, FetchRequest { chunk_hash });
                self.pending_fetches.insert(req_id, reply);
                self.fetch_targets.insert(req_id, chunk_hash);
            }
            kad::QueryResult::GetProviders(Ok(
                kad::GetProvidersOk::FinishedWithNoAdditionalRecord { .. },
            )) => {
                if let Some(reply) = self.pending_kad_lookups.remove(&id) {
                    let _ = reply.send(Err(anyhow!("kad lookup finished with no providers")));
                }
            }
            kad::QueryResult::GetProviders(Err(e)) => {
                if let Some(reply) = self.pending_kad_lookups.remove(&id) {
                    let _ = reply.send(Err(anyhow!("kad get_providers error: {e}")));
                }
            }
            _ => {}
        }
    }

    async fn handle_fetch_message(
        &mut self,
        _peer: PeerId,
        message: request_response::Message<FetchRequest, FetchResponse>,
    ) {
        match message {
            request_response::Message::Request {
                request, channel, ..
            } => {
                let src = self.local_source.clone();
                let weak = self.cmd_weak.clone();
                tokio::spawn(async move {
                    let resp = match src {
                        Some(src) => match src.read_chunk(&request.chunk_hash).await {
                            Ok(bytes) => FetchResponse::Ok { bytes },
                            Err(_) => FetchResponse::NotFound,
                        },
                        None => FetchResponse::NotFound,
                    };
                    if let Some(tx) = weak.upgrade() {
                        let _ = tx.send(Cmd::FetchRespond { channel, resp }).await;
                    }
                });
            }
            request_response::Message::Response {
                request_id,
                response,
                ..
            } => {
                if let Some(reply) = self.pending_fetches.remove(&request_id) {
                    self.fetch_targets.remove(&request_id);
                    match response {
                        FetchResponse::Ok { bytes } => {
                            let _ = reply.send(Ok(bytes));
                        }
                        FetchResponse::NotFound => {
                            let _ =
                                reply.send(Err(anyhow!("provider returned NotFound for chunk")));
                        }
                    }
                }
            }
        }
    }
}

enum EventOrCmd {
    Event(SwarmEvent<C0mputeBehaviourEvent>),
    Cmd(Cmd),
}

// hex helper since we use it in handle_kad_progress
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct InMemSource {
        bytes: Vec<u8>,
        hash: Hash,
    }

    #[async_trait]
    impl ChunkSource for InMemSource {
        async fn read_chunk(&self, hash: &Hash) -> Result<Vec<u8>> {
            if *hash == self.hash {
                Ok(self.bytes.clone())
            } else {
                Err(anyhow!("not found"))
            }
        }
    }

    fn tempdir(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "c0mpute-net-test-{}-{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// End-to-end: two libp2p swarms find each other, host A announces a
    /// chunk, host B fetches it via Kad provider lookup + request/response.
    #[tokio::test]
    async fn two_node_announce_fetch_roundtrip() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("c0mpute_net=info,libp2p_kad=warn")
            .with_test_writer()
            .try_init();

        let payload = b"hello c0mpute libp2p".to_vec();
        let h = Hash::of(&payload);

        // Node A: holds the data, will announce + serve.
        let src_a = Arc::new(InMemSource {
            bytes: payload.clone(),
            hash: h,
        });
        let net_a = Libp2pNetwork::spawn(
            NetworkConfig::for_dir(tempdir("a"))
                .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
                .with_local_source(src_a),
        )
        .await
        .unwrap();

        // Wait for A to actually be listening, then grab its listen addr.
        let a_addr = wait_for_listen(&net_a).await;
        let a_full = a_addr.with(libp2p::multiaddr::Protocol::P2p(net_a.peer_id()));

        // Node B: read-only client. Dials A using its full listen addr.
        let net_b = Libp2pNetwork::spawn(
            NetworkConfig::for_dir(tempdir("b"))
                .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
                .with_bootstrap(vec![a_full.clone()]),
        )
        .await
        .unwrap();

        // Distinct peers, sanity.
        assert_ne!(net_a.peer_id(), net_b.peer_id());

        // A announces it provides the chunk.
        net_a.announce(&h).await.unwrap();

        // Give the connection + identify exchange time to complete +
        // populate B's Kad routing table with A's address.
        tokio::time::sleep(Duration::from_millis(800)).await;

        // B fetches the chunk via Kad provider lookup → request/response.
        let bytes = net_b
            .fetch(&ChunkRequest {
                chunk_hash: h,
                shard_index: None,
            })
            .await
            .expect("fetch should succeed");
        assert_eq!(bytes, payload);
    }

    async fn wait_for_listen(net: &Libp2pNetwork) -> Multiaddr {
        for _ in 0..50 {
            let addrs = net.listen_addrs().await.unwrap_or_default();
            if let Some(a) = addrs.into_iter().next() {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("node never bound a listen addr");
    }

    struct Echo;

    #[async_trait]
    impl RpcHandler for Echo {
        async fn handle(&self, _peer: PeerId, body: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
            if body == b"fail" {
                return Err("asked to fail".into());
            }
            let mut out = b"echo:".to_vec();
            out.extend(body);
            Ok(out)
        }
    }

    /// Named-service RPC between two nodes: answers, errors, unknown service.
    #[tokio::test]
    async fn two_node_rpc_roundtrip() {
        let net_a = Libp2pNetwork::spawn(
            NetworkConfig::for_dir(tempdir("rpc-a")).with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()]),
        )
        .await
        .unwrap();
        net_a.register_rpc("echo", Arc::new(Echo));
        let a_full = wait_for_listen(&net_a)
            .await
            .with(libp2p::multiaddr::Protocol::P2p(net_a.peer_id()));
        let net_b = Libp2pNetwork::spawn(
            NetworkConfig::for_dir(tempdir("rpc-b"))
                .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
                .with_bootstrap(vec![a_full]),
        )
        .await
        .unwrap();
        let big = vec![7u8; 200_000];
        let got = net_b.rpc(net_a.peer_id(), "echo", big.clone()).await.unwrap();
        assert_eq!(&got[..5], b"echo:");
        assert_eq!(&got[5..], &big[..]);
        let err = net_b.rpc(net_a.peer_id(), "echo", b"fail".to_vec()).await.unwrap_err();
        assert!(err.to_string().contains("asked to fail"), "{err}");
        let err = net_b.rpc(net_a.peer_id(), "nope", vec![]).await.unwrap_err();
        assert!(err.to_string().contains("not served"), "{err}");
        assert!(net_b.connected_peers().await.contains(&net_a.peer_id()));
        // Calling yourself runs the local handler.
        assert_eq!(net_a.rpc(net_a.peer_id(), "echo", b"x".to_vec()).await.unwrap(), b"echo:x");
    }

    /// gossipsub publish/subscribe across two nodes.
    #[tokio::test]
    async fn two_node_gossipsub_publish_receive() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("c0mpute_net=info,libp2p_gossipsub=warn")
            .with_test_writer()
            .try_init();

        let net_a = Libp2pNetwork::spawn(
            NetworkConfig::for_dir(tempdir("gs-a"))
                .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()]),
        )
        .await
        .unwrap();
        let a_addr = wait_for_listen(&net_a).await;
        let a_full = a_addr.with(libp2p::multiaddr::Protocol::P2p(net_a.peer_id()));

        let net_b = Libp2pNetwork::spawn(
            NetworkConfig::for_dir(tempdir("gs-b"))
                .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
                .with_bootstrap(vec![a_full]),
        )
        .await
        .unwrap();

        // Both nodes subscribe to the same topic, then A publishes a
        // payload and B should receive it.
        let topic = "c0mpute/test/v1";
        net_a.subscribe(topic).await.unwrap();
        net_b.subscribe(topic).await.unwrap();

        // gossipsub needs a moment for the mesh to connect.
        tokio::time::sleep(Duration::from_millis(800)).await;

        let mut rx = net_b.messages();
        let payload = b"hello gossipsub from a".to_vec();
        net_a.publish(topic, payload.clone()).await.unwrap();

        // Wait up to 3s for delivery.
        let received = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("gossipsub message should arrive within 3s")
            .expect("broadcast channel should not be closed");

        assert_eq!(received.topic, topic);
        assert_eq!(received.data, payload);
        assert_eq!(received.source, Some(net_a.peer_id()));
    }
    /// A direct send arrives on the receiver's gossip stream, attributed to
    /// the sender, with no topic subscription or mesh at all; non-job topics
    /// and sends to ourselves are refused.
    /// Two peers that only share a third (both bootstrapped to it, never
    /// told each other's address) find and reach each other through the DHT.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn peers_known_only_through_a_third_reach_each_other() {
        // SAFETY: set before any node starts; the test owns its process env.
        unsafe { std::env::set_var("C0MPUTE_DISABLE_MDNS", "1") };
        let hub = Libp2pNetwork::spawn(
            NetworkConfig::for_dir(tempdir("reach-hub")).with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()]),
        )
        .await
        .unwrap();
        let hub_addr = wait_for_listen(&hub).await.with(libp2p::multiaddr::Protocol::P2p(hub.peer_id()));
        let mk = |name: &'static str, boot: Multiaddr| async move {
            Libp2pNetwork::spawn(
                NetworkConfig::for_dir(tempdir(name))
                    .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()])
                    .with_bootstrap(vec![boot]),
            )
            .await
            .unwrap()
        };
        let a = mk("reach-a", hub_addr.clone()).await;
        let b = mk("reach-b", hub_addr).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        // The hub's routing table knows b: a DHT lookup from a finds it.
        let found = a.find_peer(b.peer_id(), Duration::from_secs(10)).await;
        assert!(!found.is_empty(), "find_peer found b's addresses through the hub");
        a.reach(b.peer_id(), Duration::from_secs(10)).await.expect("a reaches b through the hub's routing table");
        assert!(a.is_connected(b.peer_id()).await);
        let topic = "c0mpute/jobs/diffusion.txt2img";
        b.reach(a.peer_id(), Duration::from_secs(10)).await.unwrap();
        a.send_direct(b.peer_id(), topic, b"x", Duration::from_secs(5)).await.unwrap();
    }

    #[tokio::test]
    async fn direct_send_reaches_the_peer_without_a_mesh() {
        let net_a = Libp2pNetwork::spawn(
            NetworkConfig::for_dir(tempdir("direct-a"))
                .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()]),
        )
        .await
        .unwrap();
        let a_addr = wait_for_listen(&net_a).await;
        let net_b = Libp2pNetwork::spawn(
            NetworkConfig::for_dir(tempdir("direct-b"))
                .with_listen(vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()]),
        )
        .await
        .unwrap();
        net_b.add_peer_address(net_a.peer_id(), a_addr).await;

        let mut rx = net_a.messages();
        let topic = "c0mpute/jobs/diffusion.txt2img";
        net_b
            .send_direct(net_a.peer_id(), topic, b"bid", Duration::from_secs(10))
            .await
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("direct message should arrive")
            .unwrap();
        assert_eq!(got.topic, topic);
        assert_eq!(got.data, b"bid");
        assert_eq!(got.source, Some(net_b.peer_id()));

        assert!(
            net_b
                .send_direct(net_a.peer_id(), "c0mpute/cap/v1", b"x", Duration::from_secs(5))
                .await
                .is_err()
        );
        assert!(
            net_b
                .send_direct(net_b.peer_id(), topic, b"x", Duration::from_secs(5))
                .await
                .is_err()
        );
    }

    #[test]
    fn direct_body_round_trips() {
        let body = encode_direct("c0mpute/jobs/x", b"payload");
        assert_eq!(
            decode_direct(&body),
            Some(("c0mpute/jobs/x".to_string(), b"payload".to_vec()))
        );
        assert_eq!(decode_direct(&[0]), None);
        assert_eq!(decode_direct(&[0, 9, b'a']), None);
    }
}
