//! Experimental Scallion circuit handshake: authenticated relays, forward
//! secrecy and telescoped three-hop establishment.
//!
//! This is a pure state machine. It opens no socket, starts no listener and
//! dials nobody: the caller moves [`LinkMsg`] values between peers. It is NOT
//! a working VPN, is not wired into the node, and has had no independent
//! security review. Do not expose it to users on the strength of unit tests.
//!
//! What it adds over `scallion_frames` (which stays as the v1 primitive):
//!
//! - **Authenticated relay keys.** A relay publishes a [`SignedRelayDescriptor`]:
//!   its Noise static key, roles, exit ports and expiry, signed by its libp2p
//!   identity. A circuit can only be built from a [`VerifiedRelay`], and the
//!   only way to get one is [`SignedRelayDescriptor::verify`].
//! - **Forward secrecy.** Each hop runs `Noise_NK_25519_ChaChaPoly_SHA256`
//!   (the `snow` crate, the same Noise implementation libp2p uses). The relay
//!   is authenticated by its static key, the client stays anonymous to it,
//!   and the traffic keys come from an ephemeral-ephemeral exchange, so a
//!   relay static key stolen later does not open recorded traffic.
//! - **Telescoping.** The client handshakes with the guard, then asks the
//!   guard to extend to the middle, then asks the middle (through the guard)
//!   to extend to the exit. Every hop learns its previous and next peer and
//!   nothing else. Each link carries its own random circuit id, so the id is
//!   not a global tag that links the hops together.
//! - **Sliding replay window.** Cells carry an explicit sequence number and a
//!   64-wide window, so reordered delivery is accepted and replays are not.
//!   A cell that fails authentication never advances the window.
//!
//! Spec: docs/protocol/scallion-handshake.md.
//! Threat model: docs/protocol/scallion-threat-model.md.

use anyhow::{Result, anyhow, bail, ensure};
use libp2p::identity::{Keypair, PublicKey};
use rand::{RngCore, rngs::OsRng};
use snow::{Builder, HandshakeState, StatelessTransportState, params::NoiseParams};
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};
use zeroize::Zeroizing;

const NOISE_PATTERN: &str = "Noise_NK_25519_ChaChaPoly_SHA256";
const PROLOGUE_LABEL: &[u8] = b"c0mpute/scallion/handshake/v1";
const DESCRIPTOR_DOMAIN: &[u8] = b"c0mpute/scallion/relay-descriptor/v1\0";
const DESCRIPTOR_MAGIC: &[u8; 4] = b"SC1D";
const SIGNED_MAGIC: &[u8; 4] = b"SC1S";
const LINK_MAGIC: &[u8; 4] = b"SC1L";
const DESCRIPTOR_VERSION: u8 = 1;

/// Both NK handshake messages are an ephemeral key plus one AEAD tag.
const HS_MSG_LEN: usize = 32 + 16;
const TAG_LEN: usize = 16;
/// Largest application payload in one cell.
pub const MAX_DATA: usize = 16 * 1024;
/// Upper bound on a cell ciphertext on any link (payload plus three layers).
const MAX_CELL: usize = MAX_DATA + 256;
const MAX_PEER_ID: usize = 128;
const MAX_OPERATOR: usize = 64;
const MAX_EXIT_PORTS: usize = 64;
const MAX_KEY_BYTES: usize = 512;
const MAX_SIG_BYTES: usize = 512;
const MAX_DESCRIPTOR_BYTES: usize = 1024;
/// A circuit is exactly guard, middle, exit.
pub const HOPS: usize = 3;
/// A descriptor may not be valid for longer than this.
pub const MAX_DESCRIPTOR_LIFETIME_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// Tolerated clock difference for `issued_at`.
pub const MAX_CLOCK_SKEW_MS: u64 = 5 * 60 * 1000;
const REPLAY_WINDOW: u64 = 64;

const LINK_CREATE: u8 = 1;
const LINK_CREATED: u8 = 2;
const LINK_CELL: u8 = 3;
const LINK_DESTROY: u8 = 4;

// Commands inside a forward layer (client to hop).
const CMD_FORWARD: u8 = 0x01;
const CMD_EXTEND: u8 = 0x02;
const CMD_DATA: u8 = 0x03;
const CMD_DESTROY: u8 = 0x04;
// Commands inside a backward layer (hop to client).
const CMD_RELAYED: u8 = 0x11;
const CMD_EXTENDED: u8 = 0x12;
const CMD_DATA_BACK: u8 = 0x13;
const CMD_EXTEND_FAILED: u8 = 0x14;

/// A per-link circuit id. Random, chosen by the client for every link.
pub type LinkId = [u8; 16];

// ---------------------------------------------------------------------------
// Relay descriptors
// ---------------------------------------------------------------------------

/// What a relay says it will do. `exit` is never implied by the others.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayRoles {
    pub guard: bool,
    pub middle: bool,
    pub exit: bool,
}

impl RelayRoles {
    fn bits(self) -> u8 {
        (self.guard as u8) | ((self.middle as u8) << 1) | ((self.exit as u8) << 2)
    }

    fn from_bits(bits: u8) -> Result<Self> {
        ensure!(bits & !0b111 == 0, "unknown Scallion relay role bits");
        ensure!(bits != 0, "Scallion relay descriptor declares no role");
        Ok(Self { guard: bits & 1 != 0, middle: bits & 2 != 0, exit: bits & 4 != 0 })
    }
}

/// A relay's long-lived Noise static key. Separate from its libp2p identity:
/// the identity signs it, so it can be rotated without changing the peer id.
pub struct RelayStaticKey {
    secret: StaticSecret,
}

impl RelayStaticKey {
    pub fn generate() -> Self {
        Self { secret: StaticSecret::random_from_rng(OsRng) }
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self { secret: StaticSecret::from(bytes) }
    }

    pub fn to_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.secret.to_bytes())
    }

    pub fn public(&self) -> [u8; 32] {
        XPublicKey::from(&self.secret).to_bytes()
    }
}

/// The unsigned content of a relay descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayDescriptor {
    pub peer_id: String,
    pub noise_static: [u8; 32],
    pub roles: RelayRoles,
    /// TCP ports this relay will exit to. Must be empty unless `roles.exit`.
    pub exit_ports: Vec<u16>,
    /// Self-declared operator or family label. A claim, not proof of anything.
    pub operator: String,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

impl RelayDescriptor {
    fn check(&self) -> Result<()> {
        validate_peer_id(&self.peer_id)?;
        ensure!(self.operator.len() <= MAX_OPERATOR, "Scallion operator label too long");
        ensure!(
            self.operator.bytes().all(|b| b.is_ascii_graphic() || b == b' '),
            "Scallion operator label must be printable ASCII"
        );
        ensure!(self.exit_ports.len() <= MAX_EXIT_PORTS, "too many Scallion exit ports");
        ensure!(self.exit_ports.iter().all(|&p| p != 0), "Scallion exit port 0 is not valid");
        if self.roles.exit {
            ensure!(!self.exit_ports.is_empty(), "a Scallion exit must list its allowed ports");
        } else {
            ensure!(self.exit_ports.is_empty(), "exit ports listed without the exit role");
        }
        ensure!(self.roles.bits() != 0, "Scallion relay descriptor declares no role");
        ensure!(self.expires_at_ms > self.issued_at_ms, "Scallion descriptor expires before it is issued");
        ensure!(
            self.expires_at_ms - self.issued_at_ms <= MAX_DESCRIPTOR_LIFETIME_MS,
            "Scallion descriptor lifetime too long"
        );
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.check()?;
        let mut out = Vec::with_capacity(128);
        out.extend_from_slice(DESCRIPTOR_MAGIC);
        out.push(DESCRIPTOR_VERSION);
        out.push(self.peer_id.len() as u8);
        out.extend_from_slice(self.peer_id.as_bytes());
        out.extend_from_slice(&self.noise_static);
        out.push(self.roles.bits());
        out.push(self.exit_ports.len() as u8);
        for port in &self.exit_ports {
            out.extend_from_slice(&port.to_be_bytes());
        }
        out.push(self.operator.len() as u8);
        out.extend_from_slice(self.operator.as_bytes());
        out.extend_from_slice(&self.issued_at_ms.to_be_bytes());
        out.extend_from_slice(&self.expires_at_ms.to_be_bytes());
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= MAX_DESCRIPTOR_BYTES, "Scallion descriptor too large");
        let mut r = Reader::new(bytes);
        ensure!(r.take(4)? == DESCRIPTOR_MAGIC, "not a Scallion relay descriptor");
        ensure!(r.u8()? == DESCRIPTOR_VERSION, "unsupported Scallion descriptor version");
        let peer_len = r.u8()? as usize;
        let peer_id = std::str::from_utf8(r.take(peer_len)?)
            .map_err(|_| anyhow!("Scallion descriptor peer id is not UTF-8"))?
            .to_string();
        let noise_static = r.arr32()?;
        let roles = RelayRoles::from_bits(r.u8()?)?;
        let n_ports = r.u8()? as usize;
        let mut exit_ports = Vec::with_capacity(n_ports.min(MAX_EXIT_PORTS));
        for _ in 0..n_ports {
            exit_ports.push(r.u16()?);
        }
        let op_len = r.u8()? as usize;
        let operator = std::str::from_utf8(r.take(op_len)?)
            .map_err(|_| anyhow!("Scallion operator label is not UTF-8"))?
            .to_string();
        let issued_at_ms = r.u64()?;
        let expires_at_ms = r.u64()?;
        r.finish()?;
        let descriptor =
            Self { peer_id, noise_static, roles, exit_ports, operator, issued_at_ms, expires_at_ms };
        descriptor.check()?;
        Ok(descriptor)
    }

    /// Sign with the relay's libp2p identity. The identity must be the one
    /// `peer_id` names, and the Noise key must be this relay's own.
    pub fn sign(&self, identity: &Keypair) -> Result<SignedRelayDescriptor> {
        ensure!(
            identity.public().to_peer_id().to_string() == self.peer_id,
            "Scallion descriptor peer id does not match the signing identity"
        );
        let body = self.encode()?;
        let signature = identity
            .sign(&signing_input(&body))
            .map_err(|e| anyhow!("sign Scallion descriptor: {e}"))?;
        Ok(SignedRelayDescriptor { public_key: identity.public().encode_protobuf(), body, signature })
    }
}

fn signing_input(body: &[u8]) -> Vec<u8> {
    let mut input = Vec::with_capacity(DESCRIPTOR_DOMAIN.len() + body.len());
    input.extend_from_slice(DESCRIPTOR_DOMAIN);
    input.extend_from_slice(body);
    input
}

/// A descriptor as it travels: the exact signed bytes, the signer's public
/// key and the signature. Untrusted until [`Self::verify`] succeeds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedRelayDescriptor {
    public_key: Vec<u8>,
    body: Vec<u8>,
    signature: Vec<u8>,
}

impl SignedRelayDescriptor {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.public_key.len() + self.body.len() + self.signature.len());
        out.extend_from_slice(SIGNED_MAGIC);
        for part in [&self.public_key, &self.body, &self.signature] {
            out.extend_from_slice(&(part.len() as u16).to_be_bytes());
            out.extend_from_slice(part);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= 4 + 6 + MAX_KEY_BYTES + MAX_DESCRIPTOR_BYTES + MAX_SIG_BYTES,
            "signed Scallion descriptor too large"
        );
        let mut r = Reader::new(bytes);
        ensure!(r.take(4)? == SIGNED_MAGIC, "not a signed Scallion descriptor");
        let key_len = r.u16()? as usize;
        ensure!(key_len <= MAX_KEY_BYTES, "Scallion descriptor key too large");
        let public_key = r.take(key_len)?.to_vec();
        let body_len = r.u16()? as usize;
        ensure!(body_len <= MAX_DESCRIPTOR_BYTES, "Scallion descriptor too large");
        let body = r.take(body_len)?.to_vec();
        let sig_len = r.u16()? as usize;
        ensure!(sig_len <= MAX_SIG_BYTES, "Scallion descriptor signature too large");
        let signature = r.take(sig_len)?.to_vec();
        r.finish()?;
        Ok(Self { public_key, body, signature })
    }

    /// Check the signature, that the signer is the peer the descriptor
    /// names, that it is inside its validity period at `now_ms`, and that the
    /// Noise key is usable. Time is a parameter so this stays testable.
    ///
    /// Success proves the peer's identity key vouches for this Noise key and
    /// these roles. It does NOT prove the operator label, that two relays are
    /// run by different people, or that the relay behaves.
    pub fn verify(&self, now_ms: u64) -> Result<VerifiedRelay> {
        let public = PublicKey::try_decode_protobuf(&self.public_key)
            .map_err(|_| anyhow!("Scallion descriptor public key is malformed"))?;
        let descriptor = RelayDescriptor::decode(&self.body)?;
        ensure!(
            public.to_peer_id().to_string() == descriptor.peer_id,
            "Scallion descriptor is not signed by the peer it names"
        );
        ensure!(
            public.verify(&signing_input(&self.body), &self.signature),
            "Scallion descriptor signature is invalid"
        );
        ensure!(
            descriptor.issued_at_ms <= now_ms.saturating_add(MAX_CLOCK_SKEW_MS),
            "Scallion descriptor is dated in the future"
        );
        ensure!(descriptor.expires_at_ms > now_ms, "Scallion descriptor has expired");
        ensure_contributory(&descriptor.noise_static)?;
        Ok(VerifiedRelay { descriptor })
    }
}

/// Reject the small-order points: a Diffie-Hellman with one of them yields a
/// shared secret the attacker knows.
fn ensure_contributory(public: &[u8; 32]) -> Result<()> {
    let probe = StaticSecret::random_from_rng(OsRng);
    ensure!(
        probe.diffie_hellman(&XPublicKey::from(*public)).was_contributory(),
        "Scallion relay Noise key is a low-order point"
    );
    Ok(())
}

/// A relay whose descriptor verified. There is no other way to make one, so
/// holding it means the Noise key was authenticated against the peer id.
#[derive(Clone, Debug)]
pub struct VerifiedRelay {
    descriptor: RelayDescriptor,
}

impl VerifiedRelay {
    pub fn peer_id(&self) -> &str {
        &self.descriptor.peer_id
    }

    pub fn noise_static(&self) -> &[u8; 32] {
        &self.descriptor.noise_static
    }

    pub fn roles(&self) -> RelayRoles {
        self.descriptor.roles
    }

    pub fn exit_ports(&self) -> &[u16] {
        &self.descriptor.exit_ports
    }

    pub fn operator(&self) -> &str {
        &self.descriptor.operator
    }

    pub fn expires_at_ms(&self) -> u64 {
        self.descriptor.expires_at_ms
    }
}

// ---------------------------------------------------------------------------
// Link messages
// ---------------------------------------------------------------------------

/// What travels between two adjacent peers of a circuit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkMsg {
    /// First handshake message, toward the relay being added.
    Create { link: LinkId, handshake: Vec<u8> },
    /// Second handshake message, back toward the client.
    Created { link: LinkId, handshake: Vec<u8> },
    /// One encrypted layer. The direction is who sent it on this link.
    Cell { link: LinkId, seq: u64, ciphertext: Vec<u8> },
    /// Tear the circuit down on this link.
    Destroy { link: LinkId },
}

impl LinkMsg {
    pub fn link(&self) -> LinkId {
        match self {
            Self::Create { link, .. }
            | Self::Created { link, .. }
            | Self::Cell { link, .. }
            | Self::Destroy { link } => *link,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(LINK_MAGIC);
        match self {
            Self::Create { link, handshake } => {
                out.push(LINK_CREATE);
                out.extend_from_slice(link);
                out.extend_from_slice(handshake);
            }
            Self::Created { link, handshake } => {
                out.push(LINK_CREATED);
                out.extend_from_slice(link);
                out.extend_from_slice(handshake);
            }
            Self::Cell { link, seq, ciphertext } => {
                out.push(LINK_CELL);
                out.extend_from_slice(link);
                out.extend_from_slice(&seq.to_be_bytes());
                out.extend_from_slice(&(ciphertext.len() as u32).to_be_bytes());
                out.extend_from_slice(ciphertext);
            }
            Self::Destroy { link } => {
                out.push(LINK_DESTROY);
                out.extend_from_slice(link);
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= MAX_CELL + 64, "Scallion link message too large");
        let mut r = Reader::new(bytes);
        ensure!(r.take(4)? == LINK_MAGIC, "not a Scallion link message");
        let kind = r.u8()?;
        let link = r.arr16()?;
        let msg = match kind {
            LINK_CREATE => Self::Create { link, handshake: r.take(HS_MSG_LEN)?.to_vec() },
            LINK_CREATED => Self::Created { link, handshake: r.take(HS_MSG_LEN)?.to_vec() },
            LINK_CELL => {
                let seq = r.u64()?;
                let len = r.u32()? as usize;
                ensure!(len <= MAX_CELL, "Scallion cell too large");
                ensure!(len > TAG_LEN, "Scallion cell too short");
                Self::Cell { link, seq, ciphertext: r.take(len)?.to_vec() }
            }
            LINK_DESTROY => Self::Destroy { link },
            _ => bail!("unknown Scallion link message type"),
        };
        r.finish()?;
        Ok(msg)
    }
}

// ---------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------

fn noise_params() -> Result<NoiseParams> {
    NOISE_PATTERN.parse().map_err(|e| anyhow!("Scallion Noise parameters: {e}"))
}

/// The prologue binds a handshake to the relay it is meant for and to the
/// link id, so a transcript cannot be replayed to another relay or link.
fn prologue(relay_peer: &str, link: &LinkId) -> Vec<u8> {
    let mut p = Vec::with_capacity(PROLOGUE_LABEL.len() + 1 + relay_peer.len() + 16);
    p.extend_from_slice(PROLOGUE_LABEL);
    p.push(relay_peer.len() as u8);
    p.extend_from_slice(relay_peer.as_bytes());
    p.extend_from_slice(link);
    p
}

fn validate_peer_id(peer: &str) -> Result<()> {
    ensure!(!peer.is_empty() && peer.len() <= MAX_PEER_ID, "invalid Scallion peer id length");
    ensure!(peer.bytes().all(|b| b.is_ascii_alphanumeric()), "invalid Scallion peer id");
    Ok(())
}

fn random_link() -> LinkId {
    let mut link = [0u8; 16];
    OsRng.fill_bytes(&mut link);
    link
}

fn seal(transport: &StatelessTransportState, seq: u64, plain: &[u8]) -> Result<Vec<u8>> {
    ensure!(plain.len() + TAG_LEN <= MAX_CELL, "Scallion cell too large");
    let mut out = vec![0u8; plain.len() + TAG_LEN];
    let n = transport
        .write_message(seq, plain, &mut out)
        .map_err(|e| anyhow!("Scallion encryption failed: {e}"))?;
    out.truncate(n);
    Ok(out)
}

fn open(transport: &StatelessTransportState, seq: u64, ciphertext: &[u8]) -> Result<Vec<u8>> {
    ensure!(ciphertext.len() <= MAX_CELL, "Scallion cell too large");
    ensure!(ciphertext.len() > TAG_LEN, "Scallion cell too short");
    let mut out = vec![0u8; ciphertext.len()];
    let n = transport
        .read_message(seq, ciphertext, &mut out)
        .map_err(|_| anyhow!("Scallion authentication failed"))?;
    out.truncate(n);
    Ok(out)
}

/// Outgoing sequence numbers for one direction of one hop. Never repeats.
#[derive(Default)]
struct SendCounter {
    next: u64,
}

impl SendCounter {
    fn take(&mut self) -> Result<u64> {
        // The last value is reserved by Noise for rekeying.
        ensure!(self.next < u64::MAX - 1, "Scallion sequence exhausted; build a fresh circuit");
        let seq = self.next;
        self.next += 1;
        Ok(seq)
    }
}

/// Sliding anti-replay window, as in IPsec and WireGuard: accepts each
/// sequence number once, in any order, within `REPLAY_WINDOW` of the newest.
#[derive(Default)]
struct ReplayWindow {
    started: bool,
    highest: u64,
    bitmap: u64,
}

impl ReplayWindow {
    fn would_accept(&self, seq: u64) -> bool {
        if !self.started || seq > self.highest {
            return true;
        }
        let age = self.highest - seq;
        age < REPLAY_WINDOW && self.bitmap & (1u64 << age) == 0
    }

    fn commit(&mut self, seq: u64) {
        if !self.started {
            self.started = true;
            self.highest = seq;
            self.bitmap = 1;
        } else if seq > self.highest {
            let shift = seq - self.highest;
            self.bitmap = if shift >= REPLAY_WINDOW { 0 } else { self.bitmap << shift };
            self.bitmap |= 1;
            self.highest = seq;
        } else {
            self.bitmap |= 1u64 << (self.highest - seq);
        }
    }
}

struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.buf.len() >= n, "truncated Scallion message");
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.buf)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into()?))
    }

    fn arr16(&mut self) -> Result<[u8; 16]> {
        Ok(self.take(16)?.try_into()?)
    }

    fn arr32(&mut self) -> Result<[u8; 32]> {
        Ok(self.take(32)?.try_into()?)
    }

    fn finish(&self) -> Result<()> {
        ensure!(self.buf.is_empty(), "trailing bytes in Scallion message");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Relay side
// ---------------------------------------------------------------------------

enum NextHop {
    None,
    /// CREATE sent, waiting for CREATED.
    Pending { peer: String, link: LinkId },
    Open { peer: String, link: LinkId },
}

/// What the caller must do after feeding a relay circuit a message.
#[derive(Debug, PartialEq, Eq)]
pub enum RelayAction {
    /// Send toward the client, to the peer this circuit came from.
    ToPrev(LinkMsg),
    /// Send away from the client. For a `Create`, dial `peer_id` first.
    ToNext { peer_id: String, msg: LinkMsg },
    /// A payload addressed to this hop. Opaque here: the stream layer and the
    /// exit policy (`scallion_exit`) decide what, if anything, to do with it.
    Deliver(Vec<u8>),
    /// The circuit is gone; drop its state.
    Closed,
}

/// One relay's state for one circuit.
pub struct RelayCircuit {
    local_peer: String,
    prev_peer: String,
    prev_link: LinkId,
    transport: StatelessTransportState,
    forward_replay: ReplayWindow,
    backward_seq: SendCounter,
    next: NextHop,
    closed: bool,
}

impl RelayCircuit {
    /// Answer a `Create` from `prev_peer`. Returns the circuit and the
    /// `Created` reply. `local_peer` is this relay's own peer id: a client
    /// that was aiming at a different relay fails the handshake.
    pub fn accept(
        key: &RelayStaticKey,
        local_peer: &str,
        prev_peer: &str,
        create: &LinkMsg,
    ) -> Result<(Self, LinkMsg)> {
        validate_peer_id(local_peer)?;
        ensure!(!prev_peer.is_empty() && prev_peer.len() <= MAX_PEER_ID, "invalid previous peer");
        let LinkMsg::Create { link, handshake } = create else {
            bail!("expected a Scallion Create");
        };
        ensure!(handshake.len() == HS_MSG_LEN, "Scallion Create has the wrong length");
        let private = key.to_bytes();
        let prologue = prologue(local_peer, link);
        let mut hs: HandshakeState = Builder::new(noise_params()?)
            .local_private_key(&private[..])
            .prologue(&prologue)
            .build_responder()
            .map_err(|e| anyhow!("Scallion responder setup failed: {e}"))?;
        let mut scratch = [0u8; HS_MSG_LEN];
        hs.read_message(handshake, &mut scratch)
            .map_err(|_| anyhow!("Scallion handshake rejected"))?;
        let mut reply = [0u8; HS_MSG_LEN];
        let n = hs
            .write_message(&[], &mut reply)
            .map_err(|e| anyhow!("Scallion handshake reply failed: {e}"))?;
        ensure!(n == HS_MSG_LEN, "unexpected Scallion handshake reply length");
        let transport = hs
            .into_stateless_transport_mode()
            .map_err(|e| anyhow!("Scallion handshake did not complete: {e}"))?;
        let circuit = Self {
            local_peer: local_peer.to_string(),
            prev_peer: prev_peer.to_string(),
            prev_link: *link,
            transport,
            forward_replay: ReplayWindow::default(),
            backward_seq: SendCounter::default(),
            next: NextHop::None,
            closed: false,
        };
        Ok((circuit, LinkMsg::Created { link: *link, handshake: reply.to_vec() }))
    }

    pub fn prev_peer(&self) -> &str {
        &self.prev_peer
    }

    pub fn prev_link(&self) -> LinkId {
        self.prev_link
    }

    /// The next hop of this circuit, once the client has extended it.
    pub fn next_hop(&self) -> Option<(&str, LinkId)> {
        match &self.next {
            NextHop::None => None,
            NextHop::Pending { peer, link } | NextHop::Open { peer, link } => Some((peer, *link)),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn seal_backward(&mut self, cmd: u8, body: &[u8]) -> Result<LinkMsg> {
        let mut plain = Vec::with_capacity(1 + body.len());
        plain.push(cmd);
        plain.extend_from_slice(body);
        // Encrypt before taking the number, so a refused cell wastes nothing.
        let seq = self.backward_seq.next;
        let ciphertext = seal(&self.transport, seq, &plain)?;
        self.backward_seq.take()?;
        Ok(LinkMsg::Cell { link: self.prev_link, seq, ciphertext })
    }

    fn teardown(&mut self) -> Vec<RelayAction> {
        self.closed = true;
        let mut actions = Vec::new();
        if let Some((peer, link)) = self.next_hop().map(|(p, l)| (p.to_string(), l)) {
            actions.push(RelayAction::ToNext { peer_id: peer, msg: LinkMsg::Destroy { link } });
        }
        self.next = NextHop::None;
        actions.push(RelayAction::Closed);
        actions
    }

    /// Handle a message that arrived from the client side of the circuit.
    /// An error leaves the circuit state untouched.
    pub fn handle_prev(&mut self, msg: &LinkMsg) -> Result<Vec<RelayAction>> {
        ensure!(!self.closed, "Scallion circuit is closed");
        ensure!(msg.link() == self.prev_link, "Scallion message for a different circuit");
        match msg {
            LinkMsg::Destroy { .. } => Ok(self.teardown()),
            LinkMsg::Cell { seq, ciphertext, .. } => {
                ensure!(self.forward_replay.would_accept(*seq), "replayed or stale Scallion cell");
                let plain = open(&self.transport, *seq, ciphertext)?;
                // Authentic. Validate the command before changing any state.
                let mut r = Reader::new(&plain);
                let actions = match r.u8()? {
                    CMD_FORWARD => {
                        let NextHop::Open { peer, link } = &self.next else {
                            bail!("Scallion forward on a circuit with no open next hop");
                        };
                        let inner_seq = r.u64()?;
                        let inner = r.rest();
                        ensure!(inner.len() > TAG_LEN, "Scallion forwarded cell too short");
                        vec![RelayAction::ToNext {
                            peer_id: peer.clone(),
                            msg: LinkMsg::Cell { link: *link, seq: inner_seq, ciphertext: inner.to_vec() },
                        }]
                    }
                    CMD_EXTEND => {
                        ensure!(matches!(self.next, NextHop::None), "Scallion circuit already extended");
                        let link = r.arr16()?;
                        let peer_len = r.u8()? as usize;
                        let peer = std::str::from_utf8(r.take(peer_len)?)
                            .map_err(|_| anyhow!("Scallion extend peer id is not UTF-8"))?
                            .to_string();
                        validate_peer_id(&peer)?;
                        ensure!(peer != self.local_peer, "Scallion circuit may not extend to itself");
                        ensure!(peer != self.prev_peer, "Scallion circuit may not extend back to its previous hop");
                        ensure!(link != self.prev_link, "Scallion extend reuses the incoming link id");
                        let handshake = r.take(HS_MSG_LEN)?.to_vec();
                        r.finish()?;
                        self.next = NextHop::Pending { peer: peer.clone(), link };
                        vec![RelayAction::ToNext { peer_id: peer, msg: LinkMsg::Create { link, handshake } }]
                    }
                    CMD_DATA => vec![RelayAction::Deliver(r.rest().to_vec())],
                    CMD_DESTROY => {
                        r.finish()?;
                        self.teardown()
                    }
                    _ => bail!("unknown Scallion forward command"),
                };
                self.forward_replay.commit(*seq);
                Ok(actions)
            }
            _ => bail!("unexpected Scallion message from the previous hop"),
        }
    }

    /// Handle a message that arrived from the next hop. `from_peer` is the
    /// authenticated peer id of the connection it came in on.
    pub fn handle_next(&mut self, from_peer: &str, msg: &LinkMsg) -> Result<Vec<RelayAction>> {
        ensure!(!self.closed, "Scallion circuit is closed");
        let (peer, link) = self
            .next_hop()
            .map(|(p, l)| (p.to_string(), l))
            .ok_or_else(|| anyhow!("Scallion circuit has no next hop"))?;
        ensure!(from_peer == peer, "Scallion message from a peer that is not the next hop");
        ensure!(msg.link() == link, "Scallion message for a different circuit");
        match msg {
            LinkMsg::Created { handshake, .. } => {
                ensure!(matches!(self.next, NextHop::Pending { .. }), "unexpected Scallion Created");
                ensure!(handshake.len() == HS_MSG_LEN, "Scallion Created has the wrong length");
                let cell = self.seal_backward(CMD_EXTENDED, handshake)?;
                self.next = NextHop::Open { peer, link };
                Ok(vec![RelayAction::ToPrev(cell)])
            }
            LinkMsg::Cell { seq, ciphertext, .. } => {
                ensure!(matches!(self.next, NextHop::Open { .. }), "Scallion cell before the next hop is open");
                ensure!(
                    ciphertext.len() > TAG_LEN && ciphertext.len() + 9 + TAG_LEN <= MAX_CELL,
                    "Scallion backward cell has a bad length"
                );
                let mut body = Vec::with_capacity(8 + ciphertext.len());
                body.extend_from_slice(&seq.to_be_bytes());
                body.extend_from_slice(ciphertext);
                Ok(vec![RelayAction::ToPrev(self.seal_backward(CMD_RELAYED, &body)?)])
            }
            LinkMsg::Destroy { .. } => {
                // The far side is gone. Tell the client side and drop state.
                self.next = NextHop::None;
                self.closed = true;
                Ok(vec![RelayAction::ToPrev(LinkMsg::Destroy { link: self.prev_link }), RelayAction::Closed])
            }
            _ => bail!("unexpected Scallion message from the next hop"),
        }
    }

    /// The next hop could not be reached or refused. Tell the client; the
    /// circuit stays usable up to this hop and may be extended elsewhere.
    pub fn extend_failed(&mut self) -> Result<LinkMsg> {
        ensure!(!self.closed, "Scallion circuit is closed");
        ensure!(matches!(self.next, NextHop::Pending { .. }), "no Scallion extend is pending");
        let cell = self.seal_backward(CMD_EXTEND_FAILED, &[])?;
        self.next = NextHop::None;
        Ok(cell)
    }

    /// Send a payload from this hop back to the client.
    pub fn send_data(&mut self, payload: &[u8]) -> Result<LinkMsg> {
        ensure!(!self.closed, "Scallion circuit is closed");
        ensure!(payload.len() <= MAX_DATA, "Scallion payload exceeds the cell limit");
        self.seal_backward(CMD_DATA_BACK, payload)
    }
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

struct ClientHop {
    peer_id: String,
    noise_static: [u8; 32],
    transport: StatelessTransportState,
    forward_seq: SendCounter,
    backward_replay: ReplayWindow,
}

struct PendingHop {
    handshake: HandshakeState,
    peer_id: String,
    noise_static: [u8; 32],
}

/// What happened when the client processed a message from its guard.
#[derive(Debug, PartialEq, Eq)]
pub enum ClientEvent {
    /// A hop finished its handshake. `hops` is how many are now established.
    HopReady { hops: usize },
    /// A payload from hop `hop` (0 = guard, 2 = exit).
    Data { hop: usize, payload: Vec<u8> },
    /// The last hop could not extend. Try another relay with `extend`.
    ExtendFailed,
    /// The circuit was torn down.
    Closed,
}

/// The client end of one circuit.
pub struct CircuitClient {
    guard_link: LinkId,
    hops: Vec<ClientHop>,
    pending: Option<PendingHop>,
    closed: bool,
}

impl CircuitClient {
    /// Begin a circuit at `guard`. Send the returned `Create` to the guard.
    pub fn start(guard: &VerifiedRelay) -> Result<(Self, LinkMsg)> {
        ensure!(guard.roles().guard, "the first Scallion hop must hold the guard role");
        let link = random_link();
        let (pending, handshake) = begin_handshake(guard, &link)?;
        let client = Self { guard_link: link, hops: Vec::with_capacity(HOPS), pending: Some(pending), closed: false };
        Ok((client, LinkMsg::Create { link, handshake }))
    }

    /// How many hops have completed their handshake.
    pub fn established(&self) -> usize {
        self.hops.len()
    }

    /// True once guard, middle and exit are all established.
    pub fn is_complete(&self) -> bool {
        self.hops.len() == HOPS && self.pending.is_none() && !self.closed
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// The established hops' peer ids, in order.
    pub fn path(&self) -> Vec<&str> {
        self.hops.iter().map(|h| h.peer_id.as_str()).collect()
    }

    /// Ask the last established hop to extend to `next`. Send the returned
    /// cell to the guard. The second hop must hold the middle role and the
    /// third the exit role, and every relay must be a distinct peer with a
    /// distinct key.
    pub fn extend(&mut self, next: &VerifiedRelay) -> Result<LinkMsg> {
        ensure!(!self.closed, "Scallion circuit is closed");
        ensure!(self.pending.is_none(), "a Scallion handshake is already in progress");
        ensure!(!self.hops.is_empty(), "the Scallion guard handshake has not completed");
        ensure!(self.hops.len() < HOPS, "a Scallion circuit has exactly three hops");
        if self.hops.len() == 1 {
            ensure!(next.roles().middle, "the second Scallion hop must hold the middle role");
        } else {
            ensure!(next.roles().exit, "the third Scallion hop must have opted in to the exit role");
        }
        for hop in &self.hops {
            ensure!(hop.peer_id != next.peer_id(), "repeated relay peer identity in a Scallion circuit");
            ensure!(&hop.noise_static != next.noise_static(), "repeated relay key in a Scallion circuit");
        }
        let link = random_link();
        ensure!(link != self.guard_link, "Scallion link id collision");
        let (pending, handshake) = begin_handshake(next, &link)?;
        let peer = next.peer_id().as_bytes();
        let mut body = Vec::with_capacity(16 + 1 + peer.len() + HS_MSG_LEN);
        body.extend_from_slice(&link);
        body.push(peer.len() as u8);
        body.extend_from_slice(peer);
        body.extend_from_slice(&handshake);
        let target = self.hops.len() - 1;
        let cell = self.seal_forward(target, CMD_EXTEND, &body)?;
        self.pending = Some(pending);
        Ok(cell)
    }

    /// Wrap `cmd` for hop `target`: its own layer innermost, then one
    /// forwarding layer for every hop between it and the client.
    fn seal_forward(&mut self, target: usize, cmd: u8, body: &[u8]) -> Result<LinkMsg> {
        ensure!(target < self.hops.len(), "that Scallion hop is not established");
        let mut plain = Vec::with_capacity(1 + body.len());
        plain.push(cmd);
        plain.extend_from_slice(body);
        let mut seq = self.hops[target].forward_seq.take()?;
        let mut ciphertext = seal(&self.hops[target].transport, seq, &plain)?;
        for i in (0..target).rev() {
            let mut outer = Vec::with_capacity(9 + ciphertext.len());
            outer.push(CMD_FORWARD);
            outer.extend_from_slice(&seq.to_be_bytes());
            outer.extend_from_slice(&ciphertext);
            seq = self.hops[i].forward_seq.take()?;
            ciphertext = seal(&self.hops[i].transport, seq, &outer)?;
        }
        Ok(LinkMsg::Cell { link: self.guard_link, seq, ciphertext })
    }

    /// Send a payload to hop `hop` (0 = guard, 2 = exit).
    pub fn seal_data(&mut self, hop: usize, payload: &[u8]) -> Result<LinkMsg> {
        ensure!(!self.closed, "Scallion circuit is closed");
        ensure!(payload.len() <= MAX_DATA, "Scallion payload exceeds the cell limit");
        self.seal_forward(hop, CMD_DATA, payload)
    }

    /// Tear the circuit down. Send the returned message to the guard.
    pub fn destroy(&mut self) -> LinkMsg {
        self.closed = true;
        self.pending = None;
        LinkMsg::Destroy { link: self.guard_link }
    }

    fn finish_handshake(&mut self, reply: &[u8]) -> Result<ClientEvent> {
        ensure!(reply.len() == HS_MSG_LEN, "Scallion handshake reply has the wrong length");
        let mut pending = self.pending.take().ok_or_else(|| anyhow!("no Scallion handshake is pending"))?;
        let mut scratch = [0u8; HS_MSG_LEN];
        // A wrong static key, a different relay or a tampered reply all fail
        // here. The pending handshake is dropped either way: it cannot be
        // retried with the same ephemeral.
        pending
            .handshake
            .read_message(reply, &mut scratch)
            .map_err(|_| anyhow!("Scallion relay failed to authenticate"))?;
        let transport = pending
            .handshake
            .into_stateless_transport_mode()
            .map_err(|e| anyhow!("Scallion handshake did not complete: {e}"))?;
        self.hops.push(ClientHop {
            peer_id: pending.peer_id,
            noise_static: pending.noise_static,
            transport,
            forward_seq: SendCounter::default(),
            backward_replay: ReplayWindow::default(),
        });
        Ok(ClientEvent::HopReady { hops: self.hops.len() })
    }

    /// Handle a message from the guard. An error means the message was
    /// rejected; the circuit is still usable unless `is_closed`.
    pub fn handle(&mut self, msg: &LinkMsg) -> Result<ClientEvent> {
        ensure!(!self.closed, "Scallion circuit is closed");
        ensure!(msg.link() == self.guard_link, "Scallion message for a different circuit");
        match msg {
            LinkMsg::Created { handshake, .. } => {
                ensure!(self.hops.is_empty(), "unexpected Scallion Created");
                self.finish_handshake(handshake)
            }
            LinkMsg::Destroy { .. } => {
                self.closed = true;
                self.pending = None;
                Ok(ClientEvent::Closed)
            }
            LinkMsg::Cell { seq, ciphertext, .. } => {
                let mut seq = *seq;
                let mut ciphertext = ciphertext.clone();
                let mut hop = 0usize;
                loop {
                    ensure!(hop < self.hops.len(), "Scallion cell from a hop that is not established");
                    ensure!(
                        self.hops[hop].backward_replay.would_accept(seq),
                        "replayed or stale Scallion cell"
                    );
                    let plain = open(&self.hops[hop].transport, seq, &ciphertext)?;
                    self.hops[hop].backward_replay.commit(seq);
                    let mut r = Reader::new(&plain);
                    match r.u8()? {
                        CMD_RELAYED => {
                            seq = r.u64()?;
                            ciphertext = r.rest().to_vec();
                            hop += 1;
                        }
                        CMD_EXTENDED => {
                            ensure!(hop + 1 == self.hops.len(), "Scallion Extended from the wrong hop");
                            let reply = r.rest();
                            return self.finish_handshake(reply);
                        }
                        CMD_DATA_BACK => {
                            return Ok(ClientEvent::Data { hop, payload: r.rest().to_vec() });
                        }
                        CMD_EXTEND_FAILED => {
                            ensure!(hop + 1 == self.hops.len(), "Scallion extend failure from the wrong hop");
                            ensure!(self.pending.is_some(), "no Scallion extend is pending");
                            self.pending = None;
                            return Ok(ClientEvent::ExtendFailed);
                        }
                        _ => bail!("unknown Scallion backward command"),
                    }
                }
            }
            LinkMsg::Create { .. } => bail!("unexpected Scallion Create"),
        }
    }
}

fn begin_handshake(relay: &VerifiedRelay, link: &LinkId) -> Result<(PendingHop, Vec<u8>)> {
    let prologue = prologue(relay.peer_id(), link);
    let mut handshake: HandshakeState = Builder::new(noise_params()?)
        .remote_public_key(relay.noise_static())
        .prologue(&prologue)
        .build_initiator()
        .map_err(|e| anyhow!("Scallion initiator setup failed: {e}"))?;
    let mut first = [0u8; HS_MSG_LEN];
    let n = handshake
        .write_message(&[], &mut first)
        .map_err(|e| anyhow!("Scallion handshake failed: {e}"))?;
    ensure!(n == HS_MSG_LEN, "unexpected Scallion handshake length");
    let pending =
        PendingHop { handshake, peer_id: relay.peer_id().to_string(), noise_static: *relay.noise_static() };
    Ok((pending, first.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};

    const CLIENT: &str = "client";
    const NOW: u64 = 1_800_000_000_000;

    struct Node {
        peer: String,
        key: RelayStaticKey,
        circuits: HashMap<(String, LinkId), RelayCircuit>,
        by_next: HashMap<(String, LinkId), (String, LinkId)>,
        delivered: Vec<Vec<u8>>,
        /// Every byte this relay was handed, to check what it could learn.
        seen: Vec<u8>,
    }

    impl Node {
        fn apply(&mut self, prev: (String, LinkId), actions: Vec<RelayAction>) -> Vec<(String, LinkMsg)> {
            let mut out = Vec::new();
            for action in actions {
                match action {
                    RelayAction::ToPrev(msg) => out.push((prev.0.clone(), msg)),
                    RelayAction::ToNext { peer_id, msg } => {
                        if matches!(msg, LinkMsg::Create { .. }) {
                            self.by_next.insert((peer_id.clone(), msg.link()), prev.clone());
                        }
                        out.push((peer_id, msg));
                    }
                    RelayAction::Deliver(payload) => self.delivered.push(payload),
                    RelayAction::Closed => {
                        self.circuits.remove(&prev);
                        self.by_next.retain(|_, v| *v != prev);
                    }
                }
            }
            out
        }

        fn handle(&mut self, from: &str, msg: LinkMsg) -> Result<Vec<(String, LinkMsg)>> {
            self.seen.extend_from_slice(&msg.encode());
            let key = (from.to_string(), msg.link());
            if matches!(msg, LinkMsg::Create { .. }) {
                ensure!(!self.circuits.contains_key(&key), "duplicate link id");
                let (circuit, created) = RelayCircuit::accept(&self.key, &self.peer, from, &msg)?;
                self.circuits.insert(key, circuit);
                return Ok(vec![(from.to_string(), created)]);
            }
            if self.circuits.contains_key(&key) {
                let actions = self.circuits.get_mut(&key).unwrap().handle_prev(&msg)?;
                return Ok(self.apply(key, actions));
            }
            if let Some(prev) = self.by_next.get(&key).cloned() {
                let actions = self.circuits.get_mut(&prev).unwrap().handle_next(from, &msg)?;
                return Ok(self.apply(prev, actions));
            }
            bail!("no circuit for this message")
        }
    }

    struct Net {
        nodes: HashMap<String, Node>,
        queue: VecDeque<(String, String, LinkMsg)>,
        to_client: VecDeque<LinkMsg>,
        errors: Vec<String>,
    }

    impl Net {
        fn new() -> Self {
            Self { nodes: HashMap::new(), queue: VecDeque::new(), to_client: VecDeque::new(), errors: Vec::new() }
        }

        /// Add a relay and return its verified descriptor.
        fn add(&mut self, roles: RelayRoles) -> VerifiedRelay {
            let (identity, key, signed) = relay(roles);
            let peer = identity.public().to_peer_id().to_string();
            self.nodes.insert(
                peer.clone(),
                Node {
                    peer,
                    key,
                    circuits: HashMap::new(),
                    by_next: HashMap::new(),
                    delivered: Vec::new(),
                    seen: Vec::new(),
                },
            );
            signed.verify(NOW).unwrap()
        }

        fn send(&mut self, from: &str, to: &str, msg: LinkMsg) {
            self.queue.push_back((from.to_string(), to.to_string(), msg));
        }

        fn run(&mut self) {
            while let Some((from, to, msg)) = self.queue.pop_front() {
                if to == CLIENT {
                    self.to_client.push_back(msg);
                    continue;
                }
                let node = self.nodes.get_mut(&to).expect("unknown peer");
                match node.handle(&from, msg) {
                    Ok(out) => {
                        for (dest, m) in out {
                            self.queue.push_back((to.clone(), dest, m));
                        }
                    }
                    Err(e) => self.errors.push(e.to_string()),
                }
            }
        }

        /// Deliver one client message to the guard, run the network, and
        /// hand every reply to the client.
        fn round(&mut self, client: &mut CircuitClient, guard: &str, msg: LinkMsg) -> Vec<ClientEvent> {
            self.send(CLIENT, guard, msg);
            self.run();
            let mut events = Vec::new();
            while let Some(reply) = self.to_client.pop_front() {
                events.push(client.handle(&reply).unwrap());
            }
            events
        }
    }

    fn descriptor(identity: &Keypair, key: &RelayStaticKey, roles: RelayRoles) -> RelayDescriptor {
        RelayDescriptor {
            peer_id: identity.public().to_peer_id().to_string(),
            noise_static: key.public(),
            roles,
            exit_ports: if roles.exit { vec![443] } else { vec![] },
            operator: "test-operator".to_string(),
            issued_at_ms: NOW - 1000,
            expires_at_ms: NOW + 3_600_000,
        }
    }

    fn relay(roles: RelayRoles) -> (Keypair, RelayStaticKey, SignedRelayDescriptor) {
        let identity = Keypair::generate_ed25519();
        let key = RelayStaticKey::generate();
        let signed = descriptor(&identity, &key, roles).sign(&identity).unwrap();
        (identity, key, signed)
    }

    const GUARD: RelayRoles = RelayRoles { guard: true, middle: false, exit: false };
    const MIDDLE: RelayRoles = RelayRoles { guard: false, middle: true, exit: false };
    const EXIT: RelayRoles = RelayRoles { guard: false, middle: false, exit: true };

    /// A network with one relay of each role and a complete circuit.
    fn built() -> (Net, CircuitClient, [VerifiedRelay; 3]) {
        let mut net = Net::new();
        let guard = net.add(GUARD);
        let middle = net.add(MIDDLE);
        let exit = net.add(EXIT);
        let (mut client, create) = CircuitClient::start(&guard).unwrap();
        assert_eq!(net.round(&mut client, guard.peer_id(), create), vec![ClientEvent::HopReady { hops: 1 }]);
        let cell = client.extend(&middle).unwrap();
        assert_eq!(net.round(&mut client, guard.peer_id(), cell), vec![ClientEvent::HopReady { hops: 2 }]);
        let cell = client.extend(&exit).unwrap();
        assert_eq!(net.round(&mut client, guard.peer_id(), cell), vec![ClientEvent::HopReady { hops: 3 }]);
        assert!(net.errors.is_empty(), "{:?}", net.errors);
        assert!(client.is_complete());
        (net, client, [guard, middle, exit])
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn three_hop_circuit_carries_data_both_ways() {
        let (mut net, mut client, [guard, middle, exit]) = built();
        assert_eq!(client.path(), vec![guard.peer_id(), middle.peer_id(), exit.peer_id()]);

        let cell = client.seal_data(2, b"GET / HTTP/1.1").unwrap();
        assert!(net.round(&mut client, guard.peer_id(), cell).is_empty());
        assert_eq!(net.nodes[exit.peer_id()].delivered, vec![b"GET / HTTP/1.1".to_vec()]);
        assert!(net.nodes[guard.peer_id()].delivered.is_empty());
        assert!(net.nodes[middle.peer_id()].delivered.is_empty());

        // The exit answers; the reply is wrapped by the middle and the guard.
        let exit_node = net.nodes.get_mut(exit.peer_id()).unwrap();
        let (prev, circuit) = exit_node.circuits.iter_mut().next().unwrap();
        let reply = circuit.send_data(b"HTTP/1.1 200 OK").unwrap();
        let prev_peer = prev.0.clone();
        net.send(exit.peer_id(), &prev_peer, reply);
        net.run();
        let reply = net.to_client.pop_front().unwrap();
        assert_eq!(
            client.handle(&reply).unwrap(),
            ClientEvent::Data { hop: 2, payload: b"HTTP/1.1 200 OK".to_vec() }
        );
        assert!(net.errors.is_empty(), "{:?}", net.errors);
    }

    #[test]
    fn data_can_be_addressed_to_any_hop() {
        let (mut net, mut client, [guard, middle, _exit]) = built();
        let cell = client.seal_data(0, b"to-guard").unwrap();
        net.round(&mut client, guard.peer_id(), cell);
        let cell = client.seal_data(1, b"to-middle").unwrap();
        net.round(&mut client, guard.peer_id(), cell);
        assert_eq!(net.nodes[guard.peer_id()].delivered, vec![b"to-guard".to_vec()]);
        assert_eq!(net.nodes[middle.peer_id()].delivered, vec![b"to-middle".to_vec()]);
    }

    #[test]
    fn each_hop_learns_only_its_neighbours() {
        let (mut net, mut client, [guard, middle, exit]) = built();
        let secret = b"only the exit may read this payload";
        let cell = client.seal_data(2, secret).unwrap();
        net.round(&mut client, guard.peer_id(), cell);

        let g = &net.nodes[guard.peer_id()];
        let m = &net.nodes[middle.peer_id()];
        let e = &net.nodes[exit.peer_id()];

        // The guard was told to extend to the middle and never saw the exit.
        assert!(!contains(&g.seen, exit.peer_id().as_bytes()));
        assert!(!contains(&g.seen, secret));
        let gc = g.circuits.values().next().unwrap();
        assert_eq!(gc.prev_peer(), CLIENT);
        assert_eq!(gc.next_hop().unwrap().0, middle.peer_id());

        // The middle knows the guard and the exit, never the client.
        assert!(!contains(&m.seen, secret));
        let mc = m.circuits.values().next().unwrap();
        assert_eq!(mc.prev_peer(), guard.peer_id());
        assert_eq!(mc.next_hop().unwrap().0, exit.peer_id());

        // The exit knows the middle only, and reads the payload.
        assert!(!contains(&e.seen, guard.peer_id().as_bytes()));
        let ec = e.circuits.values().next().unwrap();
        assert_eq!(ec.prev_peer(), middle.peer_id());
        assert!(ec.next_hop().is_none());
        assert_eq!(e.delivered, vec![secret.to_vec()]);

        // Every link has its own circuit id.
        let ids = [gc.prev_link(), mc.prev_link(), ec.prev_link()];
        assert!(ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2]);
    }

    #[test]
    fn descriptor_roundtrips_and_verifies() {
        let (_, key, signed) = relay(EXIT);
        let decoded = SignedRelayDescriptor::decode(&signed.encode()).unwrap();
        assert_eq!(decoded, signed);
        let verified = decoded.verify(NOW).unwrap();
        assert_eq!(verified.noise_static(), &key.public());
        assert_eq!(verified.exit_ports(), &[443]);
        assert!(verified.roles().exit && !verified.roles().guard);
        assert_eq!(verified.operator(), "test-operator");
    }

    #[test]
    fn descriptor_rejects_tampering_expiry_and_wrong_signer() {
        let (identity, key, signed) = relay(GUARD);

        // Any changed byte of the signed body breaks the signature.
        let mut bytes = signed.encode();
        let flip = bytes.len() - signed.signature.len() - 2 - 20;
        bytes[flip] ^= 1;
        match SignedRelayDescriptor::decode(&bytes) {
            Ok(tampered) => assert!(tampered.verify(NOW).is_err()),
            Err(_) => {}
        }

        // Swapping in another relay's Noise key without re-signing fails.
        let other = RelayStaticKey::generate();
        let mut forged = signed.clone();
        let offset = 4 + 1 + 1 + identity.public().to_peer_id().to_string().len();
        forged.body[offset..offset + 32].copy_from_slice(&other.public());
        assert!(forged.verify(NOW).is_err());

        // Expired, and dated in the future.
        assert!(signed.verify(NOW + 3_600_001).is_err());
        assert!(signed.verify(NOW - 1000 - MAX_CLOCK_SKEW_MS - 1).is_err());

        // A descriptor naming one peer cannot be signed by another.
        let stranger = Keypair::generate_ed25519();
        assert!(descriptor(&identity, &key, GUARD).sign(&stranger).is_err());
        let mut stolen = signed.clone();
        stolen.public_key = stranger.public().encode_protobuf();
        assert!(stolen.verify(NOW).is_err());
    }

    #[test]
    fn descriptor_rejects_bad_content() {
        let identity = Keypair::generate_ed25519();
        let key = RelayStaticKey::generate();
        let good = descriptor(&identity, &key, EXIT);

        let mut no_ports = good.clone();
        no_ports.exit_ports.clear();
        assert!(no_ports.sign(&identity).is_err(), "an exit must list ports");

        let mut ports_without_role = descriptor(&identity, &key, MIDDLE);
        ports_without_role.exit_ports = vec![443];
        assert!(ports_without_role.sign(&identity).is_err());

        let mut too_long = good.clone();
        too_long.expires_at_ms = too_long.issued_at_ms + MAX_DESCRIPTOR_LIFETIME_MS + 1;
        assert!(too_long.sign(&identity).is_err());

        let mut no_role = good.clone();
        no_role.roles = RelayRoles::default();
        no_role.exit_ports.clear();
        assert!(no_role.sign(&identity).is_err());

        let mut low_order = good.clone();
        low_order.noise_static = [0u8; 32];
        let signed = low_order.sign(&identity).unwrap();
        assert!(signed.verify(NOW).is_err(), "the all-zero point must be refused");

        assert!(SignedRelayDescriptor::decode(b"SC1S").is_err());
        assert!(SignedRelayDescriptor::decode(&[0u8; 4096]).is_err());
    }

    #[test]
    fn handshake_fails_against_the_wrong_relay_key() {
        // The descriptor is genuine, but the box answering holds another key.
        let mut net = Net::new();
        let guard = net.add(GUARD);
        net.nodes.get_mut(guard.peer_id()).unwrap().key = RelayStaticKey::generate();
        let (mut client, create) = CircuitClient::start(&guard).unwrap();
        net.send(CLIENT, guard.peer_id(), create);
        net.run();
        // Either the relay rejects the first message or the client rejects
        // the reply; the circuit must not come up.
        if let Some(reply) = net.to_client.pop_front() {
            assert!(client.handle(&reply).is_err());
        }
        assert_eq!(client.established(), 0);
    }

    #[test]
    fn handshake_is_bound_to_the_relay_peer_id() {
        // Same Noise key, but the relay believes it is a different peer.
        let (identity, key, signed) = relay(GUARD);
        let guard = signed.verify(NOW).unwrap();
        let (mut client, create) = CircuitClient::start(&guard).unwrap();
        let other_peer = Keypair::generate_ed25519().public().to_peer_id().to_string();
        match RelayCircuit::accept(&key, &other_peer, CLIENT, &create) {
            Err(_) => {}
            Ok((_, created)) => assert!(client.handle(&created).is_err()),
        }
        assert_eq!(client.established(), 0);
        // With its real peer id the same message is accepted.
        let real = identity.public().to_peer_id().to_string();
        let (mut fresh, create) = CircuitClient::start(&guard).unwrap();
        let (_, created) = RelayCircuit::accept(&key, &real, CLIENT, &create).unwrap();
        assert_eq!(fresh.handle(&created).unwrap(), ClientEvent::HopReady { hops: 1 });
    }

    #[test]
    fn replayed_and_tampered_cells_are_rejected_without_losing_state() {
        let (mut net, mut client, [guard, _middle, exit]) = built();
        let cell = client.seal_data(2, b"one").unwrap();
        net.round(&mut client, guard.peer_id(), cell.clone());
        assert_eq!(net.nodes[exit.peer_id()].delivered.len(), 1);

        // Replay of the same cell is refused at the guard.
        net.round(&mut client, guard.peer_id(), cell);
        assert_eq!(net.errors.len(), 1, "{:?}", net.errors);
        assert_eq!(net.nodes[exit.peer_id()].delivered.len(), 1);

        // A flipped ciphertext bit is refused and does not burn the number.
        let good = client.seal_data(2, b"two").unwrap();
        let LinkMsg::Cell { link, seq, mut ciphertext } = good.clone() else { unreachable!() };
        ciphertext[3] ^= 0x40;
        net.round(&mut client, guard.peer_id(), LinkMsg::Cell { link, seq, ciphertext });
        assert_eq!(net.errors.len(), 2);
        net.round(&mut client, guard.peer_id(), good);
        assert_eq!(net.errors.len(), 2, "the genuine cell must still be accepted");
        assert_eq!(net.nodes[exit.peer_id()].delivered.len(), 2);
    }

    #[test]
    fn reordered_cells_are_accepted_once_each() {
        let (mut net, mut client, [guard, _middle, exit]) = built();
        let a = client.seal_data(2, b"a").unwrap();
        let b = client.seal_data(2, b"b").unwrap();
        let c = client.seal_data(2, b"c").unwrap();
        for cell in [c.clone(), a.clone(), b.clone()] {
            net.round(&mut client, guard.peer_id(), cell);
        }
        assert!(net.errors.is_empty(), "{:?}", net.errors);
        assert_eq!(net.nodes[exit.peer_id()].delivered, vec![b"c".to_vec(), b"a".to_vec(), b"b".to_vec()]);
        for cell in [a, b, c] {
            net.round(&mut client, guard.peer_id(), cell);
        }
        assert_eq!(net.errors.len(), 3);
        assert_eq!(net.nodes[exit.peer_id()].delivered.len(), 3);
    }

    #[test]
    fn replay_window_edges() {
        let mut w = ReplayWindow::default();
        assert!(w.would_accept(0));
        w.commit(0);
        assert!(!w.would_accept(0));
        w.commit(100);
        assert!(w.would_accept(99));
        assert!(w.would_accept(100 - (REPLAY_WINDOW - 1)));
        assert!(!w.would_accept(100 - REPLAY_WINDOW), "older than the window");
        assert!(!w.would_accept(0));
        w.commit(99);
        assert!(!w.would_accept(99));
        assert!(!w.would_accept(100));
        w.commit(100 + REPLAY_WINDOW + 5);
        assert!(!w.would_accept(100), "window moved past it");
        assert!(w.would_accept(100 + REPLAY_WINDOW + 4));
    }

    #[test]
    fn client_refuses_bad_paths() {
        let mut net = Net::new();
        let guard = net.add(GUARD);
        let middle = net.add(MIDDLE);
        let exit = net.add(EXIT);

        // Roles are enforced per position.
        assert!(CircuitClient::start(&middle).is_err());
        let (mut client, create) = CircuitClient::start(&guard).unwrap();
        assert!(client.extend(&middle).is_err(), "guard handshake not finished");
        net.round(&mut client, guard.peer_id(), create);
        assert!(client.extend(&exit).is_err(), "second hop must be a middle");
        let cell = client.extend(&middle).unwrap();
        assert!(client.extend(&exit).is_err(), "one handshake at a time");
        net.round(&mut client, guard.peer_id(), cell);
        assert!(client.extend(&middle).is_err(), "third hop must be an exit");

        // The same relay may not appear twice, whatever roles it claims.
        let all = RelayRoles { guard: true, middle: true, exit: true };
        let mut net2 = Net::new();
        let both = net2.add(all);
        let (mut c2, create) = CircuitClient::start(&both).unwrap();
        net2.round(&mut c2, both.peer_id(), create);
        assert!(c2.extend(&both).is_err());

        // No fourth hop.
        let cell = client.extend(&exit).unwrap();
        net.round(&mut client, guard.peer_id(), cell);
        assert!(client.is_complete());
        let extra = net.add(EXIT);
        assert!(client.extend(&extra).is_err());
        assert!(client.seal_data(3, b"x").is_err());
        assert!(client.seal_data(2, &vec![0u8; MAX_DATA + 1]).is_err());
    }

    #[test]
    fn relay_refuses_a_second_extend_and_loops() {
        let (identity, key, signed) = relay(GUARD);
        let guard = signed.verify(NOW).unwrap();
        let local = identity.public().to_peer_id().to_string();
        let (mut client, create) = CircuitClient::start(&guard).unwrap();
        let prev = Keypair::generate_ed25519().public().to_peer_id().to_string();
        let (mut circuit, created) = RelayCircuit::accept(&key, &local, &prev, &create).unwrap();
        client.handle(&created).unwrap();

        // Extending back to the previous hop or to itself is refused. Build
        // the cells by hand: the client API will not produce them.
        for target in [prev.clone(), local.clone()] {
            let mut body = random_link().to_vec();
            body.push(target.len() as u8);
            body.extend_from_slice(target.as_bytes());
            body.extend_from_slice(&[0u8; HS_MSG_LEN]);
            let cell = client.seal_forward(0, CMD_EXTEND, &body).unwrap();
            assert!(circuit.handle_prev(&cell).is_err());
            assert!(circuit.next_hop().is_none());
        }

        // One extend is allowed; a second one is not.
        let (_, _, signed_mid) = relay(MIDDLE);
        let middle = signed_mid.verify(NOW).unwrap();
        let cell = client.extend(&middle).unwrap();
        let actions = circuit.handle_prev(&cell).unwrap();
        assert!(matches!(&actions[0], RelayAction::ToNext { peer_id, msg: LinkMsg::Create { .. } } if peer_id == middle.peer_id()));
        let mut body = random_link().to_vec();
        let other = Keypair::generate_ed25519().public().to_peer_id().to_string();
        body.push(other.len() as u8);
        body.extend_from_slice(other.as_bytes());
        body.extend_from_slice(&[0u8; HS_MSG_LEN]);
        let cell = client.seal_forward(0, CMD_EXTEND, &body).unwrap();
        assert!(circuit.handle_prev(&cell).is_err());

        // A message claiming to come from the next hop must come from it.
        let (_, link) = circuit.next_hop().map(|(p, l)| (p.to_string(), l)).unwrap();
        let fake = LinkMsg::Created { link, handshake: vec![0u8; HS_MSG_LEN] };
        assert!(circuit.handle_next(&other, &fake).is_err());
    }

    #[test]
    fn extend_failure_is_reported_and_the_circuit_can_retry() {
        let mut net = Net::new();
        let guard = net.add(GUARD);
        let middle = net.add(MIDDLE);
        let (mut client, create) = CircuitClient::start(&guard).unwrap();
        net.round(&mut client, guard.peer_id(), create);

        // A middle the guard cannot reach: the guard reports the failure.
        let (_, _, unreachable) = relay(MIDDLE);
        let unreachable = unreachable.verify(NOW).unwrap();
        let cell = client.extend(&unreachable).unwrap();
        let node = net.nodes.get_mut(guard.peer_id()).unwrap();
        let circuit = node.circuits.values_mut().next().unwrap();
        circuit.handle_prev(&cell).unwrap();
        let failed = circuit.extend_failed().unwrap();
        assert_eq!(client.handle(&failed).unwrap(), ClientEvent::ExtendFailed);
        assert_eq!(client.established(), 1);

        // The same circuit extends to a reachable middle afterwards.
        let cell = client.extend(&middle).unwrap();
        assert_eq!(net.round(&mut client, guard.peer_id(), cell), vec![ClientEvent::HopReady { hops: 2 }]);
    }

    #[test]
    fn destroy_tears_down_every_hop() {
        let (mut net, mut client, [guard, middle, exit]) = built();
        let destroy = client.destroy();
        net.send(CLIENT, guard.peer_id(), destroy);
        net.run();
        assert!(client.is_closed());
        for relay in [&guard, &middle, &exit] {
            assert!(net.nodes[relay.peer_id()].circuits.is_empty());
        }
        assert!(client.seal_data(2, b"late").is_err());
    }

    #[test]
    fn destroy_from_the_far_end_reaches_the_client() {
        let (mut net, mut client, [_guard, middle, exit]) = built();
        let exit_node = net.nodes.get_mut(exit.peer_id()).unwrap();
        let link = exit_node.circuits.values().next().unwrap().prev_link();
        exit_node.circuits.clear();
        net.send(exit.peer_id(), middle.peer_id(), LinkMsg::Destroy { link });
        net.run();
        let msg = net.to_client.pop_front().unwrap();
        assert_eq!(client.handle(&msg).unwrap(), ClientEvent::Closed);
        assert!(client.is_closed());
        assert!(net.nodes.values().all(|n| n.circuits.is_empty()));
    }

    #[test]
    fn separate_circuits_do_not_share_keys() {
        let (mut net, mut first, [guard, middle, exit]) = built();
        let (mut second, create) = CircuitClient::start(&guard).unwrap();
        net.round(&mut second, guard.peer_id(), create);
        let cell = second.extend(&middle).unwrap();
        net.round(&mut second, guard.peer_id(), cell);
        let cell = second.extend(&exit).unwrap();
        net.round(&mut second, guard.peer_id(), cell);
        assert!(second.is_complete());

        // A cell sealed on one circuit, relabelled onto the other circuit's
        // link, fails authentication: the keys are per handshake.
        let LinkMsg::Cell { seq, ciphertext, .. } = first.seal_data(2, b"mine").unwrap() else { unreachable!() };
        let relabelled = LinkMsg::Cell { link: second.guard_link, seq: seq + 1000, ciphertext };
        let before = net.errors.len();
        net.round(&mut second, guard.peer_id(), relabelled);
        assert_eq!(net.errors.len(), before + 1);
        assert!(net.nodes[exit.peer_id()].delivered.is_empty());
    }

    #[test]
    fn link_messages_roundtrip_and_reject_garbage() {
        let link = random_link();
        let msgs = [
            LinkMsg::Create { link, handshake: vec![7u8; HS_MSG_LEN] },
            LinkMsg::Created { link, handshake: vec![9u8; HS_MSG_LEN] },
            LinkMsg::Cell { link, seq: 42, ciphertext: vec![1u8; 64] },
            LinkMsg::Destroy { link },
        ];
        for msg in &msgs {
            assert_eq!(&LinkMsg::decode(&msg.encode()).unwrap(), msg);
        }
        assert!(LinkMsg::decode(b"").is_err());
        assert!(LinkMsg::decode(b"XXXX").is_err());
        let mut truncated = msgs[0].encode();
        truncated.pop();
        assert!(LinkMsg::decode(&truncated).is_err());
        let mut trailing = msgs[3].encode();
        trailing.push(0);
        assert!(LinkMsg::decode(&trailing).is_err());
        // A cell whose declared length is over the limit is refused before
        // any allocation of that size.
        let mut huge = msgs[2].encode();
        let at = 4 + 1 + 16 + 8;
        huge[at..at + 4].copy_from_slice(&(u32::MAX).to_be_bytes());
        assert!(LinkMsg::decode(&huge).is_err());
        assert!(LinkMsg::decode(&vec![0u8; MAX_CELL + 65]).is_err());
    }
}
