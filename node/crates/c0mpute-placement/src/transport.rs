//! Moving shards between nodes (CIP-003).
//!
//! Placement is written against this trait rather than against libp2p so that
//! the two can land independently. The HTTP implementation talks to the
//! CIP-002 `/storage/v1/shards/...` endpoints, which already exist and
//! already verify what they are given — so cross-node placement works today,
//! and the libp2p `/c0mpute/shard/1.0.0` protocol becomes a second
//! implementation of the same three methods rather than a prerequisite.

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use c0mpute_proto::Hash;

use crate::peer::PeerInfo;

#[async_trait]
pub trait ShardTransport: Send + Sync {
    /// Store one shard on `peer`. The peer re-hashes and rejects a mismatch,
    /// so a corrupted transfer fails at the receiver rather than silently
    /// becoming a bad shard.
    async fn put_shard(&self, peer: &PeerInfo, hash: &Hash, bytes: &[u8]) -> Result<()>;

    /// Fetch one shard from `peer`. The caller re-hashes; never trust a peer.
    async fn get_shard(&self, peer: &PeerInfo, hash: &Hash) -> Result<Vec<u8>>;

    /// Does `peer` still hold these shards? Used by repair (CIP-005) to spot
    /// degraded blocks without transferring anything.
    async fn has_shards(&self, peer: &PeerInfo, hashes: &[Hash]) -> Result<Vec<bool>>;
}

/// Talks to a peer's CIP-002 storage API.
pub struct HttpTransport {
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new(timeout: std::time::Duration) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().timeout(timeout).build()?,
        })
    }

    fn shard_url(peer: &PeerInfo, hash: &Hash) -> String {
        format!(
            "{}/storage/v1/shards/{}",
            peer.endpoint.trim_end_matches('/'),
            hash.to_hex()
        )
    }
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new(std::time::Duration::from_secs(30)).expect("default reqwest client builds")
    }
}

#[async_trait]
impl ShardTransport for HttpTransport {
    async fn put_shard(&self, peer: &PeerInfo, hash: &Hash, bytes: &[u8]) -> Result<()> {
        let resp = self
            .client
            .put(Self::shard_url(peer, hash))
            .body(bytes.to_vec())
            .send()
            .await?;
        let status = resp.status();
        // 200 means the peer already held it — content-addressed dedup, not a
        // failure.
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "peer {} rejected shard {hash}: {status} {body}",
            peer.peer_id
        )
    }

    async fn get_shard(&self, peer: &PeerInfo, hash: &Hash) -> Result<Vec<u8>> {
        let resp = self.client.get(Self::shard_url(peer, hash)).send().await?;
        if !resp.status().is_success() {
            bail!(
                "peer {} has no shard {hash}: {}",
                peer.peer_id,
                resp.status()
            );
        }
        let bytes = resp.bytes().await?.to_vec();

        // Verify before returning. A peer that serves the wrong bytes must not
        // be able to poison a reconstruction — with k of n shards there is no
        // downstream check that would catch a substituted shard until the
        // whole block fails its hash, and then we would not know which peer
        // did it.
        let actual = Hash::of(&bytes);
        if actual != *hash {
            return Err(anyhow!(
                "peer {} served bytes hashing to {actual}, not {hash}",
                peer.peer_id
            ));
        }
        Ok(bytes)
    }

    async fn has_shards(&self, peer: &PeerInfo, hashes: &[Hash]) -> Result<Vec<bool>> {
        // One HEAD per shard. CIP-003 specifies a batched `Have` probe; over
        // HTTP that would need a new endpoint, so this is the honest version
        // until the libp2p transport lands with real batching. Fine at CIP-003
        // scale, too chatty for CIP-005's hourly scan of millions of blocks.
        let mut out = Vec::with_capacity(hashes.len());
        for h in hashes {
            let held = match self.client.head(Self::shard_url(peer, h)).send().await {
                Ok(r) => r.status().is_success(),
                Err(_) => false,
            };
            out.push(held);
        }
        Ok(out)
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod memory {
    //! In-process transport with fault injection, for tests.

    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    struct Inner {
        /// peer_id -> shard hash -> bytes
        held: HashMap<String, HashMap<String, Vec<u8>>>,
        /// Peers that reject every operation, simulating an unreachable node.
        offline: HashSet<String>,
        /// Peers that serve corrupted bytes, simulating a dishonest node.
        corrupt: HashSet<String>,
        put_calls: usize,
        get_calls: usize,
    }

    #[derive(Clone, Default)]
    pub struct MemoryTransport {
        inner: Arc<Mutex<Inner>>,
    }

    impl MemoryTransport {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn take_offline(&self, peer_id: &str) {
            self.inner.lock().unwrap().offline.insert(peer_id.into());
        }

        pub fn bring_online(&self, peer_id: &str) {
            self.inner.lock().unwrap().offline.remove(peer_id);
        }

        pub fn make_corrupt(&self, peer_id: &str) {
            self.inner.lock().unwrap().corrupt.insert(peer_id.into());
        }

        /// How many shards a peer is holding.
        pub fn shard_count(&self, peer_id: &str) -> usize {
            self.inner
                .lock()
                .unwrap()
                .held
                .get(peer_id)
                .map(|m| m.len())
                .unwrap_or(0)
        }

        pub fn peers_holding(&self, hash: &Hash) -> Vec<String> {
            let inner = self.inner.lock().unwrap();
            let key = hash.to_hex();
            let mut out: Vec<String> = inner
                .held
                .iter()
                .filter(|(_, m)| m.contains_key(&key))
                .map(|(p, _)| p.clone())
                .collect();
            out.sort();
            out
        }

        pub fn get_calls(&self) -> usize {
            self.inner.lock().unwrap().get_calls
        }

        pub fn put_calls(&self) -> usize {
            self.inner.lock().unwrap().put_calls
        }
    }

    #[async_trait]
    impl ShardTransport for MemoryTransport {
        async fn put_shard(&self, peer: &PeerInfo, hash: &Hash, bytes: &[u8]) -> Result<()> {
            let mut inner = self.inner.lock().unwrap();
            inner.put_calls += 1;
            if inner.offline.contains(&peer.peer_id) {
                bail!("peer {} is offline", peer.peer_id);
            }
            inner
                .held
                .entry(peer.peer_id.clone())
                .or_default()
                .insert(hash.to_hex(), bytes.to_vec());
            Ok(())
        }

        async fn get_shard(&self, peer: &PeerInfo, hash: &Hash) -> Result<Vec<u8>> {
            let mut inner = self.inner.lock().unwrap();
            inner.get_calls += 1;
            if inner.offline.contains(&peer.peer_id) {
                bail!("peer {} is offline", peer.peer_id);
            }
            let corrupt = inner.corrupt.contains(&peer.peer_id);
            let bytes = inner
                .held
                .get(&peer.peer_id)
                .and_then(|m| m.get(&hash.to_hex()))
                .cloned()
                .ok_or_else(|| anyhow!("peer {} has no shard {hash}", peer.peer_id))?;

            if corrupt {
                let mut bad = bytes.clone();
                if let Some(b) = bad.first_mut() {
                    *b = b.wrapping_add(1);
                }
                let actual = Hash::of(&bad);
                return Err(anyhow!(
                    "peer {} served bytes hashing to {actual}, not {hash}",
                    peer.peer_id
                ));
            }
            Ok(bytes)
        }

        async fn has_shards(&self, peer: &PeerInfo, hashes: &[Hash]) -> Result<Vec<bool>> {
            let inner = self.inner.lock().unwrap();
            if inner.offline.contains(&peer.peer_id) {
                return Ok(vec![false; hashes.len()]);
            }
            let held = inner.held.get(&peer.peer_id);
            Ok(hashes
                .iter()
                .map(|h| held.is_some_and(|m| m.contains_key(&h.to_hex())))
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::memory::MemoryTransport;
    use super::*;

    fn peer(id: &str) -> PeerInfo {
        PeerInfo {
            peer_id: id.into(),
            endpoint: format!("http://{id}:7780"),
            reputation: 1.0,
            uptime_30d: 1.0,
            free_bytes: 1 << 30,
            rtt_ms: 10,
            asn: Some(1),
            region: None,
            ip_prefix: None,
        }
    }

    #[test]
    fn shard_url_is_the_cip_002_endpoint() {
        let p = peer("a");
        let h = Hash::of(b"x");
        assert_eq!(
            HttpTransport::shard_url(&p, &h),
            format!("http://a:7780/storage/v1/shards/{}", h.to_hex())
        );
    }

    #[test]
    fn shard_url_tolerates_a_trailing_slash() {
        let mut p = peer("a");
        p.endpoint = "http://a:7780/".into();
        let h = Hash::of(b"x");
        assert!(!HttpTransport::shard_url(&p, &h).contains("//storage"));
    }

    #[tokio::test]
    async fn memory_transport_round_trips() {
        let t = MemoryTransport::new();
        let p = peer("a");
        let bytes = b"shard bytes".to_vec();
        let h = Hash::of(&bytes);

        t.put_shard(&p, &h, &bytes).await.unwrap();
        assert_eq!(t.get_shard(&p, &h).await.unwrap(), bytes);
        assert_eq!(t.has_shards(&p, &[h]).await.unwrap(), vec![true]);
        assert_eq!(t.peers_holding(&h), vec!["a".to_string()]);
    }

    #[tokio::test]
    async fn offline_peers_fail_both_ways() {
        let t = MemoryTransport::new();
        let p = peer("a");
        let bytes = b"x".to_vec();
        let h = Hash::of(&bytes);
        t.put_shard(&p, &h, &bytes).await.unwrap();

        t.take_offline("a");
        assert!(t.get_shard(&p, &h).await.is_err());
        assert!(t.put_shard(&p, &h, &bytes).await.is_err());
        assert_eq!(t.has_shards(&p, &[h]).await.unwrap(), vec![false]);

        t.bring_online("a");
        assert_eq!(t.get_shard(&p, &h).await.unwrap(), bytes);
    }

    /// A dishonest peer must be caught at the transport, not at block decode —
    /// by then we would know the block is wrong but not who broke it.
    #[tokio::test]
    async fn corrupt_peers_are_detected_at_the_transport() {
        let t = MemoryTransport::new();
        let p = peer("a");
        let bytes = b"honest bytes".to_vec();
        let h = Hash::of(&bytes);
        t.put_shard(&p, &h, &bytes).await.unwrap();

        t.make_corrupt("a");
        let err = t.get_shard(&p, &h).await.unwrap_err().to_string();
        assert!(err.contains("served bytes hashing to"), "unexpected: {err}");
        assert!(err.contains('a'), "the error should name the peer: {err}");
    }
}
