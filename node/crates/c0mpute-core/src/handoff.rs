//! Hand a small secret to the one worker that won an auction.
//!
//! Gossip topics are readable by every subscriber, so a secret (a game
//! server password, the key a world backup is encrypted with) can never ride
//! in an offer. Instead the offer carries the buyer's fresh X25519 public key
//! and, after the accept:
//!
//! ```text
//! worker → handoff_ready   its own fresh X25519 key   (gossip-signed by the worker)
//! buyer  → handoff_secret  the secret, sealed to it   (gossip-signed by the buyer)
//! ```
//!
//! The key is X25519 → HKDF-SHA256 → AES-256-GCM (`temporal::crypto`), bound
//! to the workload and job id. The buyer seals only to the bidder it
//! accepted, and only on a `handoff_ready` whose gossip source is that
//! bidder; the worker opens only a secret whose gossip source is the offer's
//! buyer. Gossipsub runs with strict signing, so a source cannot be forged.
//! Every other node sees two public keys and ciphertext.
//!
//! The same exchange `webhook.deliver` does for its request, cut down to one
//! direction and a payload of at most [`MAX_SECRET`] bytes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use c0mpute_net::topics::{JobAccept, JobBid, job_topic};
use c0mpute_net::{GossipMessage, Libp2pNetwork};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::temporal::crypto;

pub use crate::temporal::crypto::KeyPair;

/// A sealed secret must fit one gossip message with room to spare.
pub const MAX_SECRET: usize = 16 * 1024;
const REPUBLISH: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ready {
    t: String,
    job_id: String,
    worker_peer_id: String,
    client_pubkey: String,
    worker_pubkey: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Sealed {
    t: String,
    job_id: String,
    worker_peer_id: String,
    sealed: String,
}

/// A fresh key pair for one handoff.
pub fn keypair() -> KeyPair {
    crypto::keypair()
}

/// `true` for 64 lowercase hex characters (an X25519 public key).
pub fn valid_pubkey(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The shared key, bound to the workload and the job.
pub fn key(secret: &x25519_dalek::StaticSecret, peer_pub_hex: &str, workload: &str, job_id: &str) -> Result<[u8; 32]> {
    crypto::session_key(secret, peer_pub_hex, &format!("handoff:{workload}:{job_id}"))
}

fn from(msg: &GossipMessage, peer: &str) -> bool {
    msg.source.map(|p| p.to_base58()).as_deref() == Some(peer)
}

/// Worker side: announce a fresh key until the buyer's sealed secret
/// arrives (or `wait` passes), and return the opened secret.
pub async fn receive(net: &Libp2pNetwork, workload: &str, accept: &JobAccept, client_pubkey: &str, wait: Duration) -> Result<Vec<u8>> {
    if !valid_pubkey(client_pubkey) {
        bail!("the offer's client key is not an X25519 public key");
    }
    let topic = job_topic(workload);
    let me = net.peer_id().to_base58();
    let mut rx = net.messages();
    let kp = crypto::keypair();
    let k = key(&kp.secret, client_pubkey, workload, &accept.job_id)?;
    let ready = serde_json::to_vec(&Ready {
        t: "handoff_ready".into(),
        job_id: accept.job_id.clone(),
        worker_peer_id: me.clone(),
        client_pubkey: client_pubkey.to_string(),
        worker_pubkey: kp.public_hex.clone(),
    })?;
    let deadline = Instant::now() + wait;
    let mut next = Instant::now();
    loop {
        if Instant::now() >= deadline {
            bail!("the buyer never sent the sealed secret");
        }
        if Instant::now() >= next {
            let _ = net.publish(&topic, ready.clone()).await;
            next = Instant::now() + REPUBLISH;
        }
        let left = next.min(deadline).saturating_duration_since(Instant::now());
        let msg = match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(m)) => m,
            Ok(Err(broadcast::error::RecvError::Closed)) => bail!("network closed"),
            _ => continue,
        };
        if msg.topic != topic || msg.data.len() > MAX_SECRET * 2 {
            continue;
        }
        let Ok(s) = serde_json::from_slice::<Sealed>(&msg.data) else { continue };
        if s.t != "handoff_secret" || s.job_id != accept.job_id || s.worker_peer_id != me || !from(&msg, &accept.buyer_peer_id) {
            continue;
        }
        match crypto::open(&k, &s.sealed) {
            Ok(plain) if plain.len() <= MAX_SECRET => return Ok(plain),
            Ok(_) => bail!("sealed secret too large"),
            Err(e) => debug!(job_id = %accept.job_id, err = %e, "handoff: unreadable secret ignored"),
        }
    }
}

/// Buyer side of one auction's handoff. Pass [`BuyerHandoff::notify`] to
/// `JobAuction::with_accept_notify`; drop (or [`BuyerHandoff::finish`]) it
/// once the receipt is in.
pub struct BuyerHandoff {
    notify: Option<mpsc::UnboundedSender<(String, JobBid)>>,
    task: JoinHandle<()>,
}

impl BuyerHandoff {
    /// Start listening for the winner's key. `kp` is the pair whose public
    /// half went into the offer.
    pub fn spawn(net: Arc<Libp2pNetwork>, workload: &str, kp: KeyPair, secret: Vec<u8>) -> Result<Self> {
        if secret.len() > MAX_SECRET {
            bail!("secret is {} bytes; a handoff carries at most {MAX_SECRET}", secret.len());
        }
        let topic = job_topic(workload);
        let workload = workload.to_string();
        let mut rx = net.messages();
        let (tx, mut acc_rx) = mpsc::unbounded_channel::<(String, JobBid)>();
        let task = tokio::spawn(async move {
            let mut winners: HashMap<String, String> = HashMap::new();
            let mut readies: HashMap<String, Ready> = HashMap::new();
            // job_id → sealed message, republished until the task ends.
            let mut sent: HashMap<String, Vec<u8>> = HashMap::new();
            let mut tick = tokio::time::interval(REPUBLISH);
            let seal = |r: &Ready| -> Result<Vec<u8>> {
                let k = key(&kp.secret, &r.worker_pubkey, &workload, &r.job_id)?;
                Ok(serde_json::to_vec(&Sealed {
                    t: "handoff_secret".into(),
                    job_id: r.job_id.clone(),
                    worker_peer_id: r.worker_peer_id.clone(),
                    sealed: crypto::seal(&k, &secret)?,
                })?)
            };
            loop {
                tokio::select! {
                    Some((job_id, bid)) = acc_rx.recv() => {
                        if let Some(r) = readies.remove(&job_id)
                            && r.worker_peer_id == bid.bidder_peer_id
                            && let Ok(m) = seal(&r)
                        {
                            let _ = net.publish(&topic, m.clone()).await;
                            sent.insert(job_id.clone(), m);
                        }
                        winners.insert(job_id, bid.bidder_peer_id);
                    }
                    m = rx.recv() => {
                        let msg = match m {
                            Ok(m) => m,
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => return,
                        };
                        if msg.topic != topic {
                            continue;
                        }
                        let Ok(r) = serde_json::from_slice::<Ready>(&msg.data) else { continue };
                        // Seal only to the bidder we accepted, on a key it signed.
                        if r.t != "handoff_ready" || r.client_pubkey != kp.public_hex || !from(&msg, &r.worker_peer_id) || sent.contains_key(&r.job_id) || !valid_pubkey(&r.worker_pubkey) {
                            continue;
                        }
                        match winners.get(&r.job_id) {
                            Some(w) if *w == r.worker_peer_id => {
                                if let Ok(m) = seal(&r) {
                                    let _ = net.publish(&topic, m.clone()).await;
                                    sent.insert(r.job_id.clone(), m);
                                }
                            }
                            Some(_) => debug!(job_id = %r.job_id, "handoff: key from a worker we did not accept ignored"),
                            None if readies.len() < 64 => { readies.insert(r.job_id.clone(), r); }
                            None => {}
                        }
                    }
                    _ = tick.tick() => {
                        for m in sent.values() {
                            let _ = net.publish(&topic, m.clone()).await;
                        }
                    }
                }
            }
        });
        Ok(Self { notify: Some(tx), task })
    }

    /// The accept notifier for `JobAuction::with_accept_notify`.
    pub fn notify(&mut self) -> Result<mpsc::UnboundedSender<(String, JobBid)>> {
        self.notify.take().context("handoff notifier already taken")
    }

    pub fn finish(self) {}
}

impl Drop for BuyerHandoff {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_derive_one_key_bound_to_the_job() {
        let buyer = keypair();
        let worker = keypair();
        let b = key(&buyer.secret, &worker.public_hex, "gameserver.start", "job-1").unwrap();
        let w = key(&worker.secret, &buyer.public_hex, "gameserver.start", "job-1").unwrap();
        assert_eq!(b, w);
        assert_ne!(b, key(&buyer.secret, &worker.public_hex, "gameserver.start", "job-2").unwrap());
        assert_ne!(b, key(&buyer.secret, &worker.public_hex, "webhook.deliver", "job-1").unwrap());
        let sealed = crypto::seal(&b, b"hunter22").unwrap();
        assert!(!sealed.contains("hunter22"));
        assert_eq!(crypto::open(&w, &sealed).unwrap(), b"hunter22");
        let eve = keypair();
        assert!(crypto::open(&key(&eve.secret, &buyer.public_hex, "gameserver.start", "job-1").unwrap(), &sealed).is_err());
    }

    #[test]
    fn pubkeys_are_checked() {
        assert!(valid_pubkey(&keypair().public_hex));
        assert!(!valid_pubkey("zz"));
        assert!(!valid_pubkey(&"A".repeat(64)));
    }
}
