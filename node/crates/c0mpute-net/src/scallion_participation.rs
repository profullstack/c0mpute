//! Scallion relay participation: which relay roles a node ADVERTISES, and a
//! small directory of the descriptors other nodes advertise.
//!
//! Nothing here carries traffic. A node that participates builds a signed
//! [`RelayDescriptor`] saying "I would act as guard and middle" (and, only
//! when its operator opted in, "as an exit to these ports") and publishes it.
//! Nodes that hear one check it with [`SignedRelayDescriptor::verify`] and
//! remember it. No circuit is built from the directory yet: link messages
//! between real nodes, the stream layer and the exit are not implemented
//! (see docs/protocol/scallion-participation.md). This is not a VPN.
//!
//! The rules this module enforces:
//!
//! - Relay participation (guard + middle) is the default for a worker.
//! - Exit is never implied. [`Participation::with_exit`] is the only way to
//!   get the exit role, it needs an explicit port list, and a node without
//!   it builds a descriptor with `exit = false` and no exit ports.
//! - [`Participation::off`] builds no descriptor at all.
//! - A directory entry exists only for a descriptor that verified and that
//!   names the peer it arrived from.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Result, bail, ensure};

use crate::scallion_exit::ExitPolicy;
use crate::scallion_handshake::{
    MAX_EXIT_PORTS, RelayDescriptor, RelayRoles, SignedRelayDescriptor, VerifiedRelay,
};

/// The ports an exit allows when its operator opts in without naming any:
/// plain web traffic only. Chosen here, conservatively; an operator can list
/// others.
pub const DEFAULT_EXIT_PORTS: [u16; 2] = [80, 443];

/// How long a node's descriptor is valid for. Short, so a node that changes
/// its roles (or disappears) stops being listed soon.
pub const DESCRIPTOR_LIFETIME_MS: u64 = 6 * 60 * 60 * 1000;

/// A node signs a fresh descriptor once less than this much validity is left.
pub const DESCRIPTOR_REFRESH_BEFORE_MS: u64 = 3 * 60 * 60 * 1000;

/// Most relays a directory remembers.
pub const MAX_DIRECTORY_RELAYS: usize = 4096;

/// What this node offers to do for Scallion. Fields are private so the exit
/// role cannot be switched on except through [`Participation::with_exit`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Participation {
    relay: bool,
    exit_ports: Vec<u16>,
}

impl Default for Participation {
    /// Guard and middle, never exit.
    fn default() -> Self {
        Self::relay_only()
    }
}

impl Participation {
    /// Not participating: no descriptor is built or published.
    pub fn off() -> Self {
        Self {
            relay: false,
            exit_ports: Vec::new(),
        }
    }

    /// Guard and middle. The default for every worker.
    pub fn relay_only() -> Self {
        Self {
            relay: true,
            exit_ports: Vec::new(),
        }
    }

    /// Guard, middle and exit, to `ports` only. The explicit opt-in: the
    /// ports go through the same checks as [`ExitPolicy::opt_in`] (at least
    /// one, never port 0) and the descriptor limit.
    pub fn with_exit(ports: impl IntoIterator<Item = u16>) -> Result<Self> {
        let ports: BTreeSet<u16> = ports.into_iter().collect();
        ExitPolicy::opt_in(ports.iter().copied())?;
        ensure!(
            ports.len() <= MAX_EXIT_PORTS,
            "a Scallion exit may list at most {MAX_EXIT_PORTS} ports"
        );
        Ok(Self {
            relay: true,
            exit_ports: ports.into_iter().collect(),
        })
    }

    /// Whether the node advertises any role.
    pub fn relays(&self) -> bool {
        self.relay
    }

    /// Whether the node advertises the exit role.
    pub fn exits(&self) -> bool {
        self.relay && !self.exit_ports.is_empty()
    }

    /// The advertised exit ports, sorted. Empty unless [`Self::exits`]: the
    /// only constructor that fills them is [`Self::with_exit`].
    pub fn exit_ports(&self) -> &[u16] {
        &self.exit_ports
    }

    /// The roles a descriptor would carry, or `None` when not participating.
    pub fn roles(&self) -> Option<RelayRoles> {
        self.relay.then(|| RelayRoles {
            guard: true,
            middle: true,
            exit: self.exits(),
        })
    }

    /// The descriptor this node would publish at `now_ms`, unsigned. `None`
    /// when not participating: such a node publishes nothing.
    pub fn descriptor(
        &self,
        peer_id: &str,
        noise_static: [u8; 32],
        operator: &str,
        now_ms: u64,
    ) -> Option<RelayDescriptor> {
        let roles = self.roles()?;
        Some(RelayDescriptor {
            peer_id: peer_id.to_string(),
            noise_static,
            roles,
            exit_ports: self.exit_ports().to_vec(),
            operator: operator.to_string(),
            issued_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(DESCRIPTOR_LIFETIME_MS),
        })
    }
}

/// The role names of a descriptor, in circuit order. `guard` is the entry.
pub fn role_names(roles: RelayRoles) -> Vec<&'static str> {
    let mut names = Vec::with_capacity(3);
    if roles.guard {
        names.push("guard");
    }
    if roles.middle {
        names.push("middle");
    }
    if roles.exit {
        names.push("exit");
    }
    names
}

/// An operator label a descriptor accepts: printable ASCII, at most 64
/// bytes, trimmed. `None` when `raw` is empty or does not fit.
pub fn sanitize_operator(raw: &str) -> Option<String> {
    let s = raw.trim();
    (!s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_graphic() || b == b' '))
        .then(|| s.to_string())
}

/// A relay in the directory.
#[derive(Clone, Debug)]
pub struct RelayRecord {
    pub relay: VerifiedRelay,
    /// When this node last heard the relay advertise this descriptor.
    pub last_seen_ms: u64,
}

struct Entry {
    relay: VerifiedRelay,
    encoded: Vec<u8>,
    last_seen_ms: u64,
}

/// Verified relay descriptors heard from peers, in memory only. It is a
/// list of who advertises what; nothing routes through it yet.
#[derive(Default)]
pub struct RelayDirectory {
    relays: HashMap<String, Entry>,
}

impl RelayDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a signed descriptor that arrived from `source_peer`. It is kept
    /// only if it verifies at `now_ms` and names `source_peer`; a descriptor
    /// older than the one already held for that peer is refused, so an old
    /// one (say, from before the operator turned exit off) cannot be played
    /// back. Returns whether the directory changed its entry for the peer.
    pub fn accept(&mut self, source_peer: &str, encoded: &[u8], now_ms: u64) -> Result<bool> {
        if let Some(held) = self.relays.get_mut(source_peer)
            && held.encoded == encoded
        {
            // The same bytes verified before; only the expiry can have changed.
            if held.relay.expires_at_ms() > now_ms {
                held.last_seen_ms = now_ms;
                return Ok(false);
            }
            self.relays.remove(source_peer);
            bail!("Scallion descriptor has expired");
        }
        let relay = SignedRelayDescriptor::decode(encoded)?.verify(now_ms)?;
        ensure!(
            relay.peer_id() == source_peer,
            "Scallion descriptor names a different peer than the one that sent it"
        );
        if let Some(held) = self.relays.get(source_peer) {
            ensure!(
                relay.issued_at_ms() >= held.relay.issued_at_ms(),
                "Scallion descriptor is older than the one already held"
            );
        } else if self.relays.len() >= MAX_DIRECTORY_RELAYS {
            self.relays.retain(|_, e| e.relay.expires_at_ms() > now_ms);
            ensure!(
                self.relays.len() < MAX_DIRECTORY_RELAYS,
                "Scallion relay directory is full"
            );
        }
        self.relays.insert(
            source_peer.to_string(),
            Entry {
                relay,
                encoded: encoded.to_vec(),
                last_seen_ms: now_ms,
            },
        );
        Ok(true)
    }

    /// Forget `peer` (it advertised without a descriptor: it stopped
    /// participating). Returns whether it was listed.
    pub fn remove(&mut self, peer: &str) -> bool {
        self.relays.remove(peer).is_some()
    }

    /// Drop expired descriptors and relays not heard from for `max_unseen_ms`.
    pub fn prune(&mut self, now_ms: u64, max_unseen_ms: u64) {
        self.relays.retain(|_, e| {
            e.relay.expires_at_ms() > now_ms
                && now_ms.saturating_sub(e.last_seen_ms) <= max_unseen_ms
        });
    }

    /// The relays whose descriptors are still valid at `now_ms`, by peer id.
    pub fn list(&self, now_ms: u64) -> Vec<RelayRecord> {
        let mut out: Vec<RelayRecord> = self
            .relays
            .values()
            .filter(|e| e.relay.expires_at_ms() > now_ms)
            .map(|e| RelayRecord {
                relay: e.relay.clone(),
                last_seen_ms: e.last_seen_ms,
            })
            .collect();
        out.sort_by(|a, b| a.relay.peer_id().cmp(b.relay.peer_id()));
        out
    }

    pub fn len(&self) -> usize {
        self.relays.len()
    }

    pub fn is_empty(&self) -> bool {
        self.relays.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use libp2p::identity::Keypair;

    use super::*;
    use crate::scallion_handshake::RelayStaticKey;

    const NOW: u64 = 1_800_000_000_000;

    fn signed(identity: &Keypair, participation: &Participation, now_ms: u64) -> Vec<u8> {
        let peer = identity.public().to_peer_id().to_string();
        participation
            .descriptor(&peer, RelayStaticKey::generate().public(), "", now_ms)
            .expect("participating")
            .sign(identity)
            .expect("sign")
            .encode()
    }

    #[test]
    fn the_default_is_relay_on_and_exit_off() {
        let p = Participation::default();
        assert!(p.relays());
        assert!(!p.exits());
        assert!(p.exit_ports().is_empty());
        assert_eq!(
            p.roles(),
            Some(RelayRoles {
                guard: true,
                middle: true,
                exit: false
            })
        );
        assert_eq!(role_names(p.roles().unwrap()), ["guard", "middle"]);
    }

    #[test]
    fn exit_off_never_advertises_exit() {
        let id = Keypair::generate_ed25519();
        let peer = id.public().to_peer_id().to_string();
        let d = Participation::relay_only()
            .descriptor(&peer, RelayStaticKey::generate().public(), "", NOW)
            .unwrap();
        assert!(d.roles.guard && d.roles.middle);
        assert!(!d.roles.exit);
        assert!(d.exit_ports.is_empty());
        // And what a peer verifies says the same.
        let relay = d.sign(&id).unwrap().verify(NOW).unwrap();
        assert!(!relay.roles().exit);
        assert!(relay.exit_ports().is_empty());
    }

    #[test]
    fn exit_opt_in_advertises_the_exit_role_and_its_ports() {
        let id = Keypair::generate_ed25519();
        let peer = id.public().to_peer_id().to_string();
        let p = Participation::with_exit([443, 80, 443]).unwrap();
        assert!(p.exits());
        assert_eq!(p.exit_ports(), [80, 443]);
        let d = p
            .descriptor(
                &peer,
                RelayStaticKey::generate().public(),
                "example operator",
                NOW,
            )
            .unwrap();
        assert_eq!(
            d.roles,
            RelayRoles {
                guard: true,
                middle: true,
                exit: true
            }
        );
        assert_eq!(d.exit_ports, [80, 443]);
        assert_eq!(d.expires_at_ms - d.issued_at_ms, DESCRIPTOR_LIFETIME_MS);
        let relay = d.sign(&id).unwrap().verify(NOW).unwrap();
        assert_eq!(role_names(relay.roles()), ["guard", "middle", "exit"]);
        assert_eq!(relay.exit_ports(), [80, 443]);
        assert_eq!(relay.operator(), "example operator");
    }

    #[test]
    fn exit_needs_valid_ports() {
        assert!(Participation::with_exit([]).is_err());
        assert!(Participation::with_exit([0, 443]).is_err());
        assert!(Participation::with_exit(1..=(MAX_EXIT_PORTS as u16 + 1)).is_err());
        assert!(Participation::with_exit(DEFAULT_EXIT_PORTS).is_ok());
    }

    #[test]
    fn participation_off_publishes_nothing() {
        let id = Keypair::generate_ed25519();
        let peer = id.public().to_peer_id().to_string();
        let p = Participation::off();
        assert!(!p.relays() && !p.exits());
        assert_eq!(p.roles(), None);
        assert!(
            p.descriptor(&peer, RelayStaticKey::generate().public(), "", NOW)
                .is_none()
        );
    }

    #[test]
    fn a_descriptor_round_trips_through_sign_and_verify() {
        let id = Keypair::generate_ed25519();
        let peer = id.public().to_peer_id().to_string();
        let key = RelayStaticKey::generate();
        let wire = Participation::relay_only()
            .descriptor(&peer, key.public(), "", NOW)
            .unwrap()
            .sign(&id)
            .unwrap()
            .encode();
        let relay = SignedRelayDescriptor::decode(&wire)
            .unwrap()
            .verify(NOW + 1)
            .unwrap();
        assert_eq!(relay.peer_id(), peer);
        assert_eq!(relay.noise_static(), &key.public());
        assert_eq!(relay.issued_at_ms(), NOW);
        // Past its lifetime it no longer verifies.
        assert!(
            SignedRelayDescriptor::decode(&wire)
                .unwrap()
                .verify(NOW + DESCRIPTOR_LIFETIME_MS)
                .is_err()
        );
        // Nor does a tampered copy.
        let mut bad = wire.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(
            SignedRelayDescriptor::decode(&bad)
                .and_then(|s| s.verify(NOW))
                .is_err()
        );
    }

    #[test]
    fn the_directory_keeps_only_verified_descriptors_from_their_own_peer() {
        let a = Keypair::generate_ed25519();
        let b = Keypair::generate_ed25519();
        let a_peer = a.public().to_peer_id().to_string();
        let b_peer = b.public().to_peer_id().to_string();
        let mut dir = RelayDirectory::new();

        let a_wire = signed(&a, &Participation::relay_only(), NOW);
        assert!(dir.accept(&a_peer, &a_wire, NOW).unwrap());
        // Hearing the same bytes again only refreshes last-seen.
        assert!(!dir.accept(&a_peer, &a_wire, NOW + 60_000).unwrap());
        assert_eq!(dir.list(NOW + 60_000)[0].last_seen_ms, NOW + 60_000);

        // A's descriptor relayed by B is refused, as is garbage.
        assert!(dir.accept(&b_peer, &a_wire, NOW).is_err());
        assert!(dir.accept(&b_peer, b"not a descriptor", NOW).is_err());
        assert_eq!(dir.len(), 1);

        let b_wire = signed(&b, &Participation::with_exit([443]).unwrap(), NOW);
        assert!(dir.accept(&b_peer, &b_wire, NOW).unwrap());
        let listed = dir.list(NOW);
        assert_eq!(listed.len(), 2);
        let exit = listed.iter().find(|r| r.relay.peer_id() == b_peer).unwrap();
        assert!(exit.relay.roles().exit);
        assert_eq!(exit.relay.exit_ports(), [443]);
        let plain = listed.iter().find(|r| r.relay.peer_id() == a_peer).unwrap();
        assert!(!plain.relay.roles().exit);
    }

    #[test]
    fn a_newer_descriptor_replaces_an_older_one_and_not_the_reverse() {
        let id = Keypair::generate_ed25519();
        let peer = id.public().to_peer_id().to_string();
        let mut dir = RelayDirectory::new();
        let with_exit = signed(&id, &Participation::with_exit([443]).unwrap(), NOW);
        let without = signed(&id, &Participation::relay_only(), NOW + 1_000);
        assert!(dir.accept(&peer, &with_exit, NOW).unwrap());
        // The operator turned exit off: the new descriptor wins.
        assert!(dir.accept(&peer, &without, NOW + 1_000).unwrap());
        assert!(!dir.list(NOW + 1_000)[0].relay.roles().exit);
        // The old exit descriptor cannot be played back over it.
        assert!(dir.accept(&peer, &with_exit, NOW + 2_000).is_err());
        assert!(!dir.list(NOW + 2_000)[0].relay.roles().exit);
    }

    #[test]
    fn the_directory_forgets_expired_silent_and_withdrawn_relays() {
        let a = Keypair::generate_ed25519();
        let b = Keypair::generate_ed25519();
        let a_peer = a.public().to_peer_id().to_string();
        let b_peer = b.public().to_peer_id().to_string();
        let mut dir = RelayDirectory::new();
        let a_wire = signed(&a, &Participation::relay_only(), NOW);
        dir.accept(&a_peer, &a_wire, NOW).unwrap();
        dir.accept(&b_peer, &signed(&b, &Participation::relay_only(), NOW), NOW)
            .unwrap();

        // B advertises without a descriptor: it turned participation off.
        assert!(dir.remove(&b_peer));
        assert!(!dir.remove(&b_peer));

        // A goes silent for longer than allowed.
        dir.prune(NOW + 10 * 60_000, 5 * 60_000);
        assert!(dir.is_empty());

        // An expired descriptor is neither listed nor accepted again.
        dir.accept(&a_peer, &a_wire, NOW).unwrap();
        assert!(dir.list(NOW + DESCRIPTOR_LIFETIME_MS).is_empty());
        assert!(
            dir.accept(&a_peer, &a_wire, NOW + DESCRIPTOR_LIFETIME_MS)
                .is_err()
        );
        assert!(dir.is_empty());
    }

    #[test]
    fn operator_labels_are_sanitized() {
        assert_eq!(
            sanitize_operator("  example operator "),
            Some("example operator".to_string())
        );
        assert_eq!(sanitize_operator(""), None);
        assert_eq!(sanitize_operator("tab\there"), None);
        assert_eq!(sanitize_operator(&"x".repeat(65)), None);
    }
}
