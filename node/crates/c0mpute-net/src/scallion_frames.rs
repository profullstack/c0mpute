//! Experimental encrypted three-hop *frame primitive* for Scallion.
//!
//! The caller MUST authenticate each node's X25519 public key against an
//! established libp2p identity before building a circuit. This module has
//! no network listener, peer discovery, key authentication, forward-secure
//! handshake, exit service, or VPN interface. It is NOT an anonymity system.
//! Each node currently uses a long-lived X25519 key; compromise of that key
//! permits decryption of recorded setup-less frames. Forward secrecy needs
//! an authenticated ephemeral relay handshake and independent review.
//!
//! Frames are for a single ordered stream. Automatic per-circuit sender
//! counters and per-hop receiver replay counters prevent accidental nonce
//! reuse and duplicate delivery within a live process. Do not persist or
//! resume a ClientCircuit counter without an anti-rollback protocol.
//!
//! Spec: docs/protocol/scallion-frames.md.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Result, anyhow, bail, ensure};
use hkdf::Hkdf;
use rand::{RngCore, rngs::OsRng};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

const MAGIC: &[u8; 4] = b"SC0F";
const VERSION: u8 = 1;
const HEADER: usize = 4 + 1 + 1 + 16 + 32 + 8 + 4;
const AEAD_TAG: usize = 16;
const MAX_PAYLOAD: usize = 32 * 1024;
const MAX_FRAME: usize = 128 * 1024;
const MAX_NEXT_ID: usize = 128;

struct ClientLayer {
    key: [u8; 32],
    ephemeral_public: [u8; 32],
}

impl Drop for ClientLayer {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

/// Constructed only from externally authenticated, distinct relay keys.
pub struct ClientCircuit {
    circuit_id: [u8; 16],
    peer_ids: [String; 3],
    layers: [ClientLayer; 3],
    next_sequence: u64,
}

impl ClientCircuit {
    pub fn new(relays: [[u8; 32]; 3], peer_ids: [String; 3]) -> Result<Self> {
        for (i, name) in peer_ids.iter().enumerate() {
            validate_next_id(name)?;
            ensure!(peer_ids[..i].iter().all(|v| v != name), "repeated relay peer identity");
            ensure!(relays[..i].iter().all(|v| v != &relays[i]), "repeated relay X25519 key");
        }
        let mut circuit_id = [0; 16];
        OsRng.fill_bytes(&mut circuit_id);
        let mut parts = Vec::with_capacity(3);
        for (i, node_public) in relays.iter().enumerate() {
            let ephemeral = StaticSecret::random_from_rng(OsRng);
            let epk = PublicKey::from(&ephemeral).to_bytes();
            let shared = ephemeral.diffie_hellman(&PublicKey::from(*node_public));
            parts.push(ClientLayer {
                key: derive_key(shared.as_bytes(), &circuit_id, i as u8, &epk, node_public)?,
                ephemeral_public: epk,
            });
        }
        let layers: [ClientLayer; 3] = parts.try_into().map_err(|_| anyhow!("invalid route length"))?;
        Ok(Self {
            circuit_id,
            peer_ids,
            layers,
            next_sequence: 0,
        })
    }

    pub fn circuit_id(&self) -> [u8; 16] {
        self.circuit_id
    }

    /// Seal one frame with a strictly increasing sequence number.
    ///
    /// Network code must deliver frames IN ORDER to each hop. Calling seal
    /// with the same ClientCircuit never repeats a sequence/nonce.
    pub fn seal(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        ensure!(data.len() <= MAX_PAYLOAD, "Scallion payload exceeds frame limit");
        let sequence = self.next_sequence.checked_add(1)
            .ok_or_else(|| anyhow!("Scallion sequence exhausted; create a fresh circuit"))?;

        let mut inside = data.to_vec();
        for i in (0..3).rev() {
            let next = if i < 2 { Some(self.peer_ids[i + 1].as_str()) } else { None };
            let plain = encode_plain(next, &inside)?;
            inside = seal_one(
                &self.layers[i].key,
                &self.circuit_id,
                i as u8,
                &self.layers[i].ephemeral_public,
                sequence,
                &plain,
            )?;
        }
        self.next_sequence = sequence;
        Ok(inside)
    }
}

/// A relay's private X25519 identity. Must be authenticated externally and
/// rotated by an authenticated key-exchange protocol before production use.
pub struct RelaySession {
    secret: StaticSecret,
    expected_circuit: [u8; 16],
    hop_index: u8,
    seen_sequence: Option<u64>,
}

pub struct HopInstruction {
    pub next_peer: Option<String>,
    pub inner: Vec<u8>,
    pub sequence: u64,
}

impl RelaySession {
    pub fn new(secret: StaticSecret, circuit_id: [u8; 16], hop_index: u8) -> Result<Self> {
        ensure!(hop_index < 3, "Scallion hop index must be 0, 1 or 2");
        Ok(Self { secret, expected_circuit: circuit_id, hop_index, seen_sequence: None })
    }

    /// Authenticate and decrypt exactly this relay's layer. A guard learns
    /// the middle but not the exit from this packet; the exit sees the
    /// unwrapped payload. This doesn't prevent traffic correlation.
    pub fn open(&mut self, frame: &[u8]) -> Result<HopInstruction> {
        let parsed = parse_frame(frame)?;
        ensure!(parsed.circuit_id == self.expected_circuit, "wrong circuit");
        ensure!(parsed.hop_index == self.hop_index, "frame addressed to a different hop");
        if let Some(last) = self.seen_sequence {
            ensure!(parsed.sequence > last, "duplicate or out-of-order Scallion frame");
        }
        let node_public = PublicKey::from(&self.secret).to_bytes();
        let shared = self.secret.diffie_hellman(&PublicKey::from(parsed.ephemeral_public));
        let mut key = derive_key(
            shared.as_bytes(),
            &parsed.circuit_id,
            parsed.hop_index,
            &parsed.ephemeral_public,
            &node_public,
        )?;
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow!("invalid Scallion key"))?;
        let nonce_bytes = nonce(parsed.hop_index, parsed.sequence);
        let result = cipher.decrypt(Nonce::from_slice(&nonce_bytes), Payload {
            msg: parsed.ciphertext,
            aad: parsed.header,
        });
        key.zeroize();
        let plain = result.map_err(|_| anyhow!("Scallion authentication failed"))?;
        let (next_peer, inner) = parse_plain(&plain)?;
        ensure!(self.hop_index == 2 || next_peer.is_some(), "forwarding hop missing next peer");
        ensure!(self.hop_index != 2 || next_peer.is_none(), "exit frame unexpectedly forwards");
        self.seen_sequence = Some(parsed.sequence);
        Ok(HopInstruction { next_peer, inner, sequence: parsed.sequence })
    }
}

fn derive_key(
    shared: &[u8; 32],
    circuit_id: &[u8; 16],
    role: u8,
    ephemeral: &[u8; 32],
    relay: &[u8; 32],
) -> Result<[u8; 32]> {
    ensure!(shared.iter().any(|&x| x != 0), "invalid low-order relay key");
    let hk = Hkdf::<Sha256>::new(Some(circuit_id), shared);
    let mut info = b"c0mpute/scallion/experimental-forward/v1".to_vec();
    info.push(role);
    info.extend_from_slice(ephemeral);
    info.extend_from_slice(relay);
    let mut key = [0u8; 32];
    hk.expand(&info, &mut key).map_err(|_| anyhow!("Scallion HKDF failed"))?;
    Ok(key)
}

fn nonce(hop_index: u8, seq: u64) -> [u8; 12] {
    let mut v = [0u8; 12];
    v[..4].copy_from_slice(&[b'S', b'C', b'L', hop_index]);
    v[4..].copy_from_slice(&seq.to_be_bytes());
    v
}

fn header(
    circuit_id: &[u8; 16],
    hop_index: u8,
    epk: &[u8; 32],
    seq: u64,
    ciphertext_len: usize,
) -> Result<Vec<u8>> {
    ensure!(ciphertext_len <= MAX_FRAME - HEADER, "frame too long");
    let mut out = Vec::with_capacity(HEADER);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(hop_index);
    out.extend_from_slice(circuit_id);
    out.extend_from_slice(epk);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&(ciphertext_len as u32).to_be_bytes());
    Ok(out)
}

fn seal_one(
    key: &[u8; 32],
    circuit_id: &[u8; 16],
    hop_index: u8,
    epk: &[u8; 32],
    sequence: u64,
    plain: &[u8],
) -> Result<Vec<u8>> {
    let aad = header(circuit_id, hop_index, epk, sequence, plain.len() + AEAD_TAG)?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow!("invalid Scallion key"))?;
    let nonce_bytes = nonce(hop_index, sequence);
    let mut encrypted = cipher.encrypt(Nonce::from_slice(&nonce_bytes), Payload { msg: plain, aad: &aad })
        .map_err(|_| anyhow!("Scallion encryption failed"))?;
    let mut packet = aad;
    packet.append(&mut encrypted);
    Ok(packet)
}

struct ParsedFrame<'a> {
    circuit_id: [u8; 16],
    hop_index: u8,
    ephemeral_public: [u8; 32],
    sequence: u64,
    header: &'a [u8],
    ciphertext: &'a [u8],
}

fn parse_frame(data: &[u8]) -> Result<ParsedFrame<'_>> {
    ensure!(data.len() >= HEADER + AEAD_TAG && data.len() <= MAX_FRAME, "invalid frame size");
    ensure!(&data[..4] == MAGIC && data[4] == VERSION, "unknown Scallion frame version");
    let role = data[5];
    ensure!(role < 3, "invalid hop index");
    let circuit_id = data[6..22].try_into().map_err(|_| anyhow!("bad circuit ID"))?;
    let epk = data[22..54].try_into().map_err(|_| anyhow!("bad ephemeral key"))?;
    let seq = u64::from_be_bytes(data[54..62].try_into()?);
    ensure!(seq > 0, "zero sequence");
    let len = u32::from_be_bytes(data[62..66].try_into()?) as usize;
    ensure!(len >= AEAD_TAG && len == data.len() - HEADER, "invalid ciphertext length");
    Ok(ParsedFrame {
        circuit_id,
        hop_index: role,
        ephemeral_public: epk,
        sequence: seq,
        header: &data[..HEADER],
        ciphertext: &data[HEADER..],
    })
}

fn validate_next_id(s: &str) -> Result<()> {
    ensure!(!s.is_empty() && s.len() <= MAX_NEXT_ID && s.bytes().all(|b| b.is_ascii_alphanumeric()), "invalid next relay identifier");
    Ok(())
}

fn encode_plain(next: Option<&str>, inner: &[u8]) -> Result<Vec<u8>> {
    if let Some(next) = next {
        validate_next_id(next)?;
    }
    ensure!(inner.len() <= MAX_FRAME, "oversized nested frame");
    let next = next.unwrap_or("").as_bytes();
    let mut out = Vec::with_capacity(2 + next.len() + 4 + inner.len());
    out.extend_from_slice(&(next.len() as u16).to_be_bytes());
    out.extend_from_slice(next);
    out.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    out.extend_from_slice(inner);
    Ok(out)
}

fn parse_plain(input: &[u8]) -> Result<(Option<String>, Vec<u8>)> {
    ensure!(input.len() >= 6, "truncated forwarding instruction");
    let n = u16::from_be_bytes(input[..2].try_into()?) as usize;
    ensure!(n <= MAX_NEXT_ID && input.len() >= 2 + n + 4, "invalid relay identifier length");
    let name = if n == 0 { None } else {
        let s = std::str::from_utf8(&input[2..2 + n])?;
        validate_next_id(s)?;
        Some(s.to_string())
    };
    let base = 2 + n;
    let len = u32::from_be_bytes(input[base..base + 4].try_into()?) as usize;
    ensure!(len <= MAX_FRAME && len == input.len() - base - 4, "invalid inner length");
    Ok((name, input[base + 4..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (ClientCircuit, [RelaySession; 3]) {
        let secrets = [
            StaticSecret::random_from_rng(OsRng),
            StaticSecret::random_from_rng(OsRng),
            StaticSecret::random_from_rng(OsRng),
        ];
        let publics = secrets.each_ref().map(|s| PublicKey::from(s).to_bytes());
        let client = ClientCircuit::new(
            publics,
            ["Guard123".into(), "Middle456".into(), "Exit789".into()],
        ).unwrap();
        let c = client.circuit_id();
        let [a, b, d] = secrets;
        (client, [
            RelaySession::new(a, c, 0).unwrap(),
            RelaySession::new(b, c, 1).unwrap(),
            RelaySession::new(d, c, 2).unwrap(),
        ])
    }

    #[test]
    fn three_independent_relays_open_exactly_one_layer_each() {
        let (mut client, [mut guard, mut middle, mut exit]) = setup();
        let frame = client.seal(b"hello over three hops").unwrap();
        let g = guard.open(&frame).unwrap();
        assert_eq!(g.next_peer.as_deref(), Some("Middle456"));
        assert!(!g.inner.windows(10).any(|w| w == b"three hops"));
        let m = middle.open(&g.inner).unwrap();
        assert_eq!(m.next_peer.as_deref(), Some("Exit789"));
        let e = exit.open(&m.inner).unwrap();
        assert_eq!(e.next_peer, None);
        assert_eq!(e.inner, b"hello over three hops");

        let frame2 = client.seal(b"second frame").unwrap();
        let g2 = guard.open(&frame2).unwrap();
        let m2 = middle.open(&g2.inner).unwrap();
        assert_eq!(exit.open(&m2.inner).unwrap().inner, b"second frame");
    }

    #[test]
    fn duplicate_frames_are_rejected_per_hop() {
        let (mut client, [mut guard, _, _]) = setup();
        let f = client.seal(b"first").unwrap();
        guard.open(&f).unwrap();
        assert!(guard.open(&f).is_err());
    }

    #[test]
    fn tampering_and_wrong_circuit_are_rejected() {
        let (mut client, [mut guard, _, _]) = setup();
        let mut f = client.seal(b"keep secret").unwrap();
        let tail = f.len() - 1;
        f[tail] ^= 1;
        assert!(guard.open(&f).is_err());
        let mut wrong_circuit = client.seal(b"next").unwrap();
        wrong_circuit[6] ^= 1;
        assert!(guard.open(&wrong_circuit).is_err());
        let mut with_garbage = client.seal(b"good").unwrap();
        with_garbage.push(0);
        assert!(guard.open(&with_garbage).is_err());
    }

    #[test]
    fn wrong_relay_key_cannot_open_frame() {
        let (mut client, [_, _, _]) = setup();
        let frame = client.seal(b"secret").unwrap();
        let mut bad = RelaySession::new(
            StaticSecret::random_from_rng(OsRng), client.circuit_id(), 0,
        ).unwrap();
        assert!(bad.open(&frame).is_err());
    }

    #[test]
    fn refuses_repeated_peer_identifiers_or_public_keys() {
        let s = StaticSecret::random_from_rng(OsRng);
        let p = PublicKey::from(&s).to_bytes();
        assert!(ClientCircuit::new([p, p, p], ["a".into(), "b".into(), "c".into()]).is_err());
        let keys = [
            PublicKey::from(&StaticSecret::random_from_rng(OsRng)).to_bytes(),
            PublicKey::from(&StaticSecret::random_from_rng(OsRng)).to_bytes(),
            PublicKey::from(&StaticSecret::random_from_rng(OsRng)).to_bytes(),
        ];
        assert!(ClientCircuit::new(keys, ["a".into(), "a".into(), "c".into()]).is_err());
    }

    #[test]
    fn zero_and_low_order_public_keys_fail() {
        let valid = [
            PublicKey::from(&StaticSecret::random_from_rng(OsRng)).to_bytes(),
            PublicKey::from(&StaticSecret::random_from_rng(OsRng)).to_bytes(),
        ];
        let invalid = [0u8; 32];
        assert!(ClientCircuit::new([valid[0], valid[1], invalid], ["a".into(), "b".into(), "c".into()]).is_err());
    }

    #[test]
    fn invalid_frame_lengths_are_rejected() {
        let (mut client, [mut guard, _, _]) = setup();
        let frame = client.seal(b"hello").unwrap();
        assert!(guard.open(&frame[..HEADER - 1]).is_err());
        assert!(client.seal(&vec![0u8; MAX_PAYLOAD + 1]).is_err());
    }
}
