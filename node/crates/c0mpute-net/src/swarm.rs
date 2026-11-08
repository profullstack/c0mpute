//! libp2p swarm for c0mpute.
//!
//! Phase 1 surface:
//!   - tcp + noise + yamux transport
//!   - kad DHT under `/c0mpute/kad/1.0.0` for peer + content discovery
//!   - identify + ping for housekeeping
//!   - request/response under `/c0mpute/chunk-fetch/1.0.0` for chunk bytes
//!
//! The swarm runs on a tokio task; the `Network` trait impl talks to it
//! through mpsc channels.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
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
}

impl Libp2pNetwork {
    /// Spawn the swarm task and return a handle. The task runs until the
    /// returned `Libp2pNetwork` is dropped.
    pub async fn spawn(config: NetworkConfig) -> Result<Self> {
        let keypair = id_mod::load_or_create(&config.identity_dir)?;
        let peer_id = keypair.public().to_peer_id();

        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(64);
        let (gossip_tx, _) = broadcast::channel::<GossipMessage>(256);

        let task = SwarmTask::new(keypair, config.local_source, gossip_tx.clone()).await?;
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

        Ok(Self {
            cmd_tx,
            gossip_tx,
            peer_id,
        })
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
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
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
    /// Broadcast channel for gossipsub messages delivered to subscribed
    /// topics. Senders are upstream consumers that called `messages()`.
    gossip_tx: broadcast::Sender<GossipMessage>,
}

impl SwarmTask {
    async fn new(
        keypair: identity::Keypair,
        local_source: Option<Arc<dyn ChunkSource>>,
        gossip_tx: broadcast::Sender<GossipMessage>,
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
                let gossipsub_cfg = gossipsub::ConfigBuilder::default()
                    .heartbeat_interval(Duration::from_secs(1))
                    .validation_mode(gossipsub::ValidationMode::Strict)
                    .build()
                    .map_err(|e| std::io::Error::other(format!("gossipsub config: {e}")))?;
                let gossipsub = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Signed(key.clone()),
                    gossipsub_cfg,
                )
                .map_err(|e| std::io::Error::other(format!("gossipsub behaviour: {e}")))?;
                let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?;
                Ok(C0mputeBehaviour {
                    kad,
                    identify,
                    ping,
                    fetch,
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
                    // Add to gossipsub mesh too — gossipsub itself uses
                    // Kad for routing once peers are known.
                    sw.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                }
            }
            SwarmEvent::Behaviour(C0mputeBehaviourEvent::Mdns(mdns::Event::Expired(peers))) => {
                let mut sw = self.swarm.lock().await;
                for (peer_id, _addr) in peers {
                    debug!(%peer_id, "mDNS peer expired");
                    sw.behaviour_mut().gossipsub.remove_explicit_peer(&peer_id);
                }
            }
            _ => {}
        }
    }

    async fn handle_kad_progress(&mut self, id: kad::QueryId, result: kad::QueryResult) {
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
                let resp = match &self.local_source {
                    Some(src) => match src.read_chunk(&request.chunk_hash).await {
                        Ok(bytes) => FetchResponse::Ok { bytes },
                        Err(_) => FetchResponse::NotFound,
                    },
                    None => FetchResponse::NotFound,
                };
                let mut sw = self.swarm.lock().await;
                let _ = sw.behaviour_mut().fetch.send_response(channel, resp);
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
}
