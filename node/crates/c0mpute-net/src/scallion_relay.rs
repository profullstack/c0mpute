//! Experimental Scallion relay table: every circuit one relay carries, with
//! the limits a relay needs before it can face the network.
//!
//! Like `scallion_handshake`, this is a pure state machine: it opens no
//! socket and reads no clock. The caller hands it each [`LinkMsg`] with the
//! authenticated peer id of the connection it arrived on and the current
//! time, and sends what comes back. It is NOT wired into the node, has had no
//! independent review and is not a working VPN.
//!
//! What it enforces:
//!
//! - a cap on circuits in total and per previous peer;
//! - a rate limit on `Create` per peer (a handshake costs two DH operations);
//! - an idle timeout, and a shorter timeout for an extend that never
//!   completes;
//! - link ids that are unique per peer, in both directions;
//! - teardown of every circuit through a peer that disconnected.
//!
//! Spec: docs/protocol/scallion-handshake.md ("Relay limits").

use std::collections::HashMap;

use anyhow::{Result, bail, ensure};

use crate::scallion_handshake::{LinkId, LinkMsg, RelayAction, RelayCircuit, RelayStaticKey};

/// A circuit as this relay names it: the peer it came from and that link's id.
pub type CircuitKey = (String, LinkId);

/// Bounds on what one relay will carry. The defaults are conservative
/// starting points, not measured values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayLimits {
    pub max_circuits: usize,
    pub max_circuits_per_peer: usize,
    /// A circuit with no traffic for this long is torn down.
    pub idle_timeout_ms: u64,
    /// An extend whose `Created` has not arrived after this long has failed.
    pub extend_timeout_ms: u64,
    /// At most this many `Create` messages per peer in each window.
    pub max_creates_per_window: u32,
    pub create_window_ms: u64,
}

impl Default for RelayLimits {
    fn default() -> Self {
        Self {
            max_circuits: 1024,
            max_circuits_per_peer: 16,
            idle_timeout_ms: 120_000,
            extend_timeout_ms: 15_000,
            max_creates_per_window: 20,
            create_window_ms: 10_000,
        }
    }
}

/// What the caller must do with the result of feeding the table.
#[derive(Debug, PartialEq, Eq)]
pub enum Outbound {
    /// Send `msg` to `peer_id`. For a `Create`, dial the peer first; if that
    /// fails, call [`RelayTable::extend_unreachable`].
    Send { peer_id: String, msg: LinkMsg },
    /// A payload addressed to this relay on `circuit`. Opaque here.
    Deliver { circuit: CircuitKey, payload: Vec<u8> },
}

struct Entry {
    circuit: RelayCircuit,
    last_active_ms: u64,
    extend_started_ms: Option<u64>,
}

/// All of one relay's circuits.
pub struct RelayTable {
    local_peer: String,
    key: RelayStaticKey,
    limits: RelayLimits,
    circuits: HashMap<CircuitKey, Entry>,
    /// (next peer, next link id) to the circuit that owns that link.
    by_next: HashMap<CircuitKey, CircuitKey>,
    per_peer: HashMap<String, usize>,
    /// Per peer: start of the current window and `Create`s seen in it.
    create_rate: HashMap<String, (u64, u32)>,
}

impl RelayTable {
    pub fn new(local_peer: &str, key: RelayStaticKey, limits: RelayLimits) -> Self {
        Self {
            local_peer: local_peer.to_string(),
            key,
            limits,
            circuits: HashMap::new(),
            by_next: HashMap::new(),
            per_peer: HashMap::new(),
            create_rate: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.circuits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.circuits.is_empty()
    }

    /// Circuits currently held for `peer` as their previous hop.
    pub fn circuits_from(&self, peer: &str) -> usize {
        self.per_peer.get(peer).copied().unwrap_or(0)
    }

    fn allow_create(&mut self, peer: &str, now_ms: u64) -> bool {
        let window = self.limits.create_window_ms;
        let slot = self.create_rate.entry(peer.to_string()).or_insert((now_ms, 0));
        if now_ms.saturating_sub(slot.0) >= window {
            *slot = (now_ms, 0);
        }
        if slot.1 >= self.limits.max_creates_per_window {
            return false;
        }
        slot.1 += 1;
        true
    }

    fn remove(&mut self, key: &CircuitKey) {
        if self.circuits.remove(key).is_some() {
            if let Some(count) = self.per_peer.get_mut(&key.0) {
                *count -= 1;
                if *count == 0 {
                    self.per_peer.remove(&key.0);
                }
            }
        }
        self.by_next.retain(|_, owner| owner != key);
    }

    /// Turn a circuit's actions into things to send, keeping the indexes in
    /// step. `key` is the circuit the actions came from.
    fn apply(&mut self, key: &CircuitKey, actions: Vec<RelayAction>, now_ms: u64) -> Result<Vec<Outbound>> {
        let mut out = Vec::with_capacity(actions.len());
        for action in actions {
            match action {
                RelayAction::ToPrev(msg) => out.push(Outbound::Send { peer_id: key.0.clone(), msg }),
                RelayAction::ToNext { peer_id, msg } => {
                    if matches!(msg, LinkMsg::Create { .. }) {
                        let next_key = (peer_id.clone(), msg.link());
                        // The client picked this id. If this relay already
                        // uses it with that peer, in either direction, the
                        // extend cannot proceed.
                        if self.by_next.contains_key(&next_key) || self.circuits.contains_key(&next_key) {
                            let entry = self.circuits.get_mut(key).expect("circuit exists");
                            let failed = entry.circuit.extend_failed()?;
                            out.push(Outbound::Send { peer_id: key.0.clone(), msg: failed });
                            continue;
                        }
                        self.by_next.insert(next_key, key.clone());
                        if let Some(entry) = self.circuits.get_mut(key) {
                            entry.extend_started_ms = Some(now_ms);
                        }
                    }
                    out.push(Outbound::Send { peer_id, msg });
                }
                RelayAction::Deliver(payload) => out.push(Outbound::Deliver { circuit: key.clone(), payload }),
                RelayAction::Closed => self.remove(key),
            }
        }
        Ok(out)
    }

    /// Handle one link message from `from_peer`, the authenticated peer id of
    /// the connection it arrived on. An error means the message was refused;
    /// the table is unchanged apart from rate accounting.
    pub fn handle(&mut self, from_peer: &str, msg: &LinkMsg, now_ms: u64) -> Result<Vec<Outbound>> {
        let key: CircuitKey = (from_peer.to_string(), msg.link());

        if matches!(msg, LinkMsg::Create { .. }) {
            ensure!(self.allow_create(from_peer, now_ms), "Scallion Create rate limit reached for this peer");
            ensure!(self.circuits.len() < self.limits.max_circuits, "this Scallion relay is at its circuit limit");
            ensure!(
                self.circuits_from(from_peer) < self.limits.max_circuits_per_peer,
                "this peer is at its Scallion circuit limit"
            );
            ensure!(
                !self.circuits.contains_key(&key) && !self.by_next.contains_key(&key),
                "Scallion link id already in use with this peer"
            );
            let (circuit, created) = RelayCircuit::accept(&self.key, &self.local_peer, from_peer, msg)?;
            self.circuits.insert(key.clone(), Entry { circuit, last_active_ms: now_ms, extend_started_ms: None });
            *self.per_peer.entry(key.0.clone()).or_insert(0) += 1;
            return Ok(vec![Outbound::Send { peer_id: key.0, msg: created }]);
        }

        if let Some(entry) = self.circuits.get_mut(&key) {
            let actions = entry.circuit.handle_prev(msg)?;
            entry.last_active_ms = now_ms;
            return self.apply(&key, actions, now_ms);
        }

        if let Some(owner) = self.by_next.get(&key).cloned() {
            let Some(entry) = self.circuits.get_mut(&owner) else {
                self.by_next.remove(&key);
                bail!("Scallion circuit is gone");
            };
            let actions = entry.circuit.handle_next(from_peer, msg)?;
            entry.last_active_ms = now_ms;
            if matches!(msg, LinkMsg::Created { .. }) {
                entry.extend_started_ms = None;
            }
            return self.apply(&owner, actions, now_ms);
        }

        bail!("no Scallion circuit for this message")
    }

    /// Send a payload from this relay back to the client of `circuit`.
    pub fn send_data(&mut self, circuit: &CircuitKey, payload: &[u8], now_ms: u64) -> Result<Outbound> {
        let entry = self.circuits.get_mut(circuit).ok_or_else(|| anyhow::anyhow!("no such Scallion circuit"))?;
        let msg = entry.circuit.send_data(payload)?;
        entry.last_active_ms = now_ms;
        Ok(Outbound::Send { peer_id: circuit.0.clone(), msg })
    }

    /// The `Create` this table asked the caller to send to `next_peer` on
    /// `link` could not be delivered. Tells the client; the circuit stays.
    pub fn extend_unreachable(&mut self, next_peer: &str, link: LinkId) -> Result<Outbound> {
        let next_key = (next_peer.to_string(), link);
        let owner = self.by_next.remove(&next_key).ok_or_else(|| anyhow::anyhow!("no such pending Scallion extend"))?;
        let entry = self.circuits.get_mut(&owner).ok_or_else(|| anyhow::anyhow!("Scallion circuit is gone"))?;
        let msg = entry.circuit.extend_failed()?;
        entry.extend_started_ms = None;
        Ok(Outbound::Send { peer_id: owner.0, msg })
    }

    /// Tear down every circuit that used `peer` as its previous or next hop.
    pub fn peer_disconnected(&mut self, peer: &str, now_ms: u64) -> Vec<Outbound> {
        let mut out = Vec::new();
        // Circuits that came from the peer: tell the far side.
        let from: Vec<CircuitKey> = self.circuits.keys().filter(|k| k.0 == peer).cloned().collect();
        for key in from {
            let destroy = LinkMsg::Destroy { link: key.1 };
            if let Some(entry) = self.circuits.get_mut(&key) {
                if let Ok(actions) = entry.circuit.handle_prev(&destroy) {
                    out.extend(self.apply(&key, actions, now_ms).unwrap_or_default());
                }
            }
            self.remove(&key);
        }
        // Circuits that went on to the peer: tell the client side.
        let toward: Vec<(CircuitKey, CircuitKey)> =
            self.by_next.iter().filter(|(next, _)| next.0 == peer).map(|(n, o)| (n.clone(), o.clone())).collect();
        for (next_key, owner) in toward {
            let destroy = LinkMsg::Destroy { link: next_key.1 };
            if let Some(entry) = self.circuits.get_mut(&owner) {
                if let Ok(actions) = entry.circuit.handle_next(peer, &destroy) {
                    out.extend(self.apply(&owner, actions, now_ms).unwrap_or_default());
                }
            }
            self.remove(&owner);
        }
        self.create_rate.remove(peer);
        // Nothing can be sent to a peer that is gone.
        out.retain(|o| !matches!(o, Outbound::Send { peer_id, .. } if peer_id == peer));
        out
    }

    /// Apply the timeouts. Call periodically.
    pub fn expire(&mut self, now_ms: u64) -> Vec<Outbound> {
        let mut out = Vec::new();

        let stuck: Vec<CircuitKey> = self
            .circuits
            .iter()
            .filter(|(_, e)| {
                e.extend_started_ms.is_some_and(|t| now_ms.saturating_sub(t) >= self.limits.extend_timeout_ms)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in stuck {
            let Some(entry) = self.circuits.get_mut(&key) else { continue };
            let next = entry.circuit.next_hop().map(|(p, l)| (p.to_string(), l));
            if let Ok(failed) = entry.circuit.extend_failed() {
                entry.extend_started_ms = None;
                out.push(Outbound::Send { peer_id: key.0.clone(), msg: failed });
                if let Some((peer_id, link)) = next {
                    self.by_next.remove(&(peer_id.clone(), link));
                    // In case the next relay did accept and its reply was lost.
                    out.push(Outbound::Send { peer_id, msg: LinkMsg::Destroy { link } });
                }
            }
        }

        let idle: Vec<CircuitKey> = self
            .circuits
            .iter()
            .filter(|(_, e)| now_ms.saturating_sub(e.last_active_ms) >= self.limits.idle_timeout_ms)
            .map(|(k, _)| k.clone())
            .collect();
        for key in idle {
            let destroy = LinkMsg::Destroy { link: key.1 };
            out.push(Outbound::Send { peer_id: key.0.clone(), msg: destroy.clone() });
            if let Some(entry) = self.circuits.get_mut(&key) {
                if let Ok(actions) = entry.circuit.handle_prev(&destroy) {
                    out.extend(self.apply(&key, actions, now_ms).unwrap_or_default());
                }
            }
            self.remove(&key);
        }

        let window = self.limits.create_window_ms;
        self.create_rate.retain(|_, (start, _)| now_ms.saturating_sub(*start) < window);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scallion_handshake::{
        CircuitClient, ClientEvent, RelayDescriptor, RelayRoles, VerifiedRelay,
    };
    use libp2p::identity::Keypair;
    use std::collections::VecDeque;

    const CLIENT: &str = "client";
    const NOW: u64 = 1_800_000_000_000;

    const GUARD: RelayRoles = RelayRoles { guard: true, middle: false, exit: false };
    const MIDDLE: RelayRoles = RelayRoles { guard: false, middle: true, exit: false };
    const EXIT: RelayRoles = RelayRoles { guard: false, middle: false, exit: true };

    fn relay(roles: RelayRoles, limits: RelayLimits) -> (VerifiedRelay, RelayTable) {
        let identity = Keypair::generate_ed25519();
        let key = RelayStaticKey::generate();
        let peer = identity.public().to_peer_id().to_string();
        let verified = RelayDescriptor {
            peer_id: peer.clone(),
            noise_static: key.public(),
            roles,
            exit_ports: if roles.exit { vec![443] } else { vec![] },
            operator: "test".to_string(),
            issued_at_ms: NOW - 1000,
            expires_at_ms: NOW + 3_600_000,
        }
        .sign(&identity)
        .unwrap()
        .verify(NOW)
        .unwrap();
        (verified, RelayTable::new(&peer, key, limits))
    }

    struct Net {
        tables: HashMap<String, RelayTable>,
        queue: VecDeque<(String, String, LinkMsg)>,
        to_client: VecDeque<LinkMsg>,
        delivered: Vec<(String, CircuitKey, Vec<u8>)>,
        errors: Vec<String>,
        /// Peers that drop everything sent to them.
        dead: Vec<String>,
        now: u64,
    }

    impl Net {
        fn new() -> Self {
            Self {
                tables: HashMap::new(),
                queue: VecDeque::new(),
                to_client: VecDeque::new(),
                delivered: Vec::new(),
                errors: Vec::new(),
                dead: Vec::new(),
                now: NOW,
            }
        }

        fn add(&mut self, roles: RelayRoles, limits: RelayLimits) -> VerifiedRelay {
            let (verified, table) = relay(roles, limits);
            self.tables.insert(verified.peer_id().to_string(), table);
            verified
        }

        fn push(&mut self, from: &str, out: Vec<Outbound>) {
            for o in out {
                match o {
                    Outbound::Send { peer_id, msg } => self.queue.push_back((from.to_string(), peer_id, msg)),
                    Outbound::Deliver { circuit, payload } => self.delivered.push((from.to_string(), circuit, payload)),
                }
            }
        }

        fn run(&mut self) {
            while let Some((from, to, msg)) = self.queue.pop_front() {
                if to == CLIENT {
                    self.to_client.push_back(msg);
                    continue;
                }
                if self.dead.contains(&to) {
                    continue;
                }
                let now = self.now;
                match self.tables.get_mut(&to).expect("unknown peer").handle(&from, &msg, now) {
                    Ok(out) => self.push(&to, out),
                    Err(e) => self.errors.push(e.to_string()),
                }
            }
        }

        fn round(&mut self, client: &mut CircuitClient, guard: &str, msg: LinkMsg) -> Vec<ClientEvent> {
            self.queue.push_back((CLIENT.to_string(), guard.to_string(), msg));
            self.run();
            let mut events = Vec::new();
            while let Some(reply) = self.to_client.pop_front() {
                events.push(client.handle(&reply).unwrap());
            }
            events
        }

        fn drain_client(&mut self, client: &mut CircuitClient) -> Vec<ClientEvent> {
            self.run();
            let mut events = Vec::new();
            while let Some(reply) = self.to_client.pop_front() {
                events.push(client.handle(&reply).unwrap());
            }
            events
        }
    }

    fn built(limits: RelayLimits) -> (Net, CircuitClient, [VerifiedRelay; 3]) {
        let mut net = Net::new();
        let guard = net.add(GUARD, limits);
        let middle = net.add(MIDDLE, limits);
        let exit = net.add(EXIT, limits);
        let (mut client, create) = CircuitClient::start(&guard).unwrap();
        net.round(&mut client, guard.peer_id(), create);
        let cell = client.extend(&middle).unwrap();
        net.round(&mut client, guard.peer_id(), cell);
        let cell = client.extend(&exit).unwrap();
        net.round(&mut client, guard.peer_id(), cell);
        assert!(net.errors.is_empty(), "{:?}", net.errors);
        assert!(client.is_complete());
        (net, client, [guard, middle, exit])
    }

    #[test]
    fn tables_carry_a_circuit_and_route_replies() {
        let (mut net, mut client, [guard, _middle, exit]) = built(RelayLimits::default());
        for table in net.tables.values() {
            assert_eq!(table.len(), 1);
        }
        let cell = client.seal_data(2, b"ping").unwrap();
        net.round(&mut client, guard.peer_id(), cell);
        assert_eq!(net.delivered.len(), 1);
        let (at, circuit, payload) = net.delivered[0].clone();
        assert_eq!(at, exit.peer_id());
        assert_eq!(payload, b"ping");

        let now = net.now;
        let reply = net.tables.get_mut(exit.peer_id()).unwrap().send_data(&circuit, b"pong", now).unwrap();
        net.push(exit.peer_id(), vec![reply]);
        assert_eq!(net.drain_client(&mut client), vec![ClientEvent::Data { hop: 2, payload: b"pong".to_vec() }]);
        assert!(net.errors.is_empty(), "{:?}", net.errors);
    }

    #[test]
    fn per_peer_and_total_circuit_caps() {
        let limits = RelayLimits { max_circuits_per_peer: 2, max_creates_per_window: 100, ..RelayLimits::default() };
        let (guard, mut table) = relay(GUARD, limits);
        for i in 0..3 {
            let (_, create) = CircuitClient::start(&guard).unwrap();
            let result = table.handle(CLIENT, &create, NOW);
            assert_eq!(result.is_ok(), i < 2, "circuit {i}");
        }
        assert_eq!(table.circuits_from(CLIENT), 2);
        // Another peer still gets its own allowance.
        let (_, create) = CircuitClient::start(&guard).unwrap();
        assert!(table.handle("otherclient", &create, NOW).is_ok());

        let limits = RelayLimits { max_circuits: 2, max_creates_per_window: 100, ..RelayLimits::default() };
        let (guard, mut table) = relay(GUARD, limits);
        for (i, peer) in ["a", "b", "c"].iter().enumerate() {
            let (_, create) = CircuitClient::start(&guard).unwrap();
            assert_eq!(table.handle(peer, &create, NOW).is_ok(), i < 2);
        }
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn create_rate_limit_resets_with_the_window() {
        let limits = RelayLimits {
            max_creates_per_window: 3,
            create_window_ms: 1000,
            max_circuits_per_peer: 100,
            ..RelayLimits::default()
        };
        let (guard, mut table) = relay(GUARD, limits);
        let mut ok = 0;
        for _ in 0..5 {
            let (_, create) = CircuitClient::start(&guard).unwrap();
            if table.handle(CLIENT, &create, NOW).is_ok() {
                ok += 1;
            }
        }
        assert_eq!(ok, 3);
        // A refused Create costs the relay no handshake and no circuit.
        assert_eq!(table.len(), 3);
        let (_, create) = CircuitClient::start(&guard).unwrap();
        assert!(table.handle(CLIENT, &create, NOW + 999).is_err());
        assert!(table.handle(CLIENT, &create, NOW + 1000).is_ok());
    }

    #[test]
    fn duplicate_link_ids_and_unknown_circuits_are_refused() {
        let (guard, mut table) = relay(GUARD, RelayLimits::default());
        let (_, create) = CircuitClient::start(&guard).unwrap();
        assert!(table.handle(CLIENT, &create, NOW).is_ok());
        assert!(table.handle(CLIENT, &create, NOW).is_err(), "same peer, same link id");
        assert_eq!(table.len(), 1);
        let stray = LinkMsg::Cell { link: [9u8; 16], seq: 0, ciphertext: vec![0u8; 32] };
        assert!(table.handle(CLIENT, &stray, NOW).is_err());
        // The same link id from another peer is a different circuit.
        assert!(table.handle("otherclient", &create, NOW).is_ok());
    }

    #[test]
    fn idle_circuits_expire_on_every_hop() {
        let limits = RelayLimits { idle_timeout_ms: 1000, ..RelayLimits::default() };
        let (mut net, mut client, [guard, middle, exit]) = built(limits);
        net.now = NOW + 999;
        for peer in [guard.peer_id(), middle.peer_id(), exit.peer_id()] {
            assert!(net.tables.get_mut(peer).unwrap().expire(NOW + 999).is_empty());
        }
        // The guard notices first and tells both sides.
        net.now = NOW + 1000;
        let out = net.tables.get_mut(guard.peer_id()).unwrap().expire(NOW + 1000);
        net.push(guard.peer_id(), out);
        assert_eq!(net.drain_client(&mut client), vec![ClientEvent::Closed]);
        assert!(client.is_closed());
        assert!(net.tables.values().all(|t| t.is_empty()));
        assert!(net.errors.is_empty(), "{:?}", net.errors);
    }

    #[test]
    fn traffic_keeps_a_circuit_alive() {
        let limits = RelayLimits { idle_timeout_ms: 1000, ..RelayLimits::default() };
        let (mut net, mut client, [guard, _middle, _exit]) = built(limits);
        net.now = NOW + 900;
        let cell = client.seal_data(2, b"keepalive").unwrap();
        net.round(&mut client, guard.peer_id(), cell);
        for table in net.tables.values_mut() {
            assert!(table.expire(NOW + 1500).is_empty());
            assert_eq!(table.len(), 1);
        }
    }

    #[test]
    fn a_stuck_extend_times_out_and_the_client_can_retry() {
        let limits = RelayLimits { extend_timeout_ms: 500, ..RelayLimits::default() };
        let mut net = Net::new();
        let guard = net.add(GUARD, limits);
        let silent = net.add(MIDDLE, limits);
        let middle = net.add(MIDDLE, limits);
        net.dead.push(silent.peer_id().to_string());

        let (mut client, create) = CircuitClient::start(&guard).unwrap();
        net.round(&mut client, guard.peer_id(), create);
        let cell = client.extend(&silent).unwrap();
        assert!(net.round(&mut client, guard.peer_id(), cell).is_empty());

        assert!(net.tables.get_mut(guard.peer_id()).unwrap().expire(NOW + 499).is_empty());
        let out = net.tables.get_mut(guard.peer_id()).unwrap().expire(NOW + 500);
        net.push(guard.peer_id(), out);
        assert_eq!(net.drain_client(&mut client), vec![ClientEvent::ExtendFailed]);
        assert_eq!(net.tables[guard.peer_id()].len(), 1, "the circuit itself survives");

        let cell = client.extend(&middle).unwrap();
        assert_eq!(net.round(&mut client, guard.peer_id(), cell), vec![ClientEvent::HopReady { hops: 2 }]);
        assert!(net.errors.is_empty(), "{:?}", net.errors);
    }

    #[test]
    fn an_undeliverable_create_is_reported_at_once() {
        let mut net = Net::new();
        let guard = net.add(GUARD, RelayLimits::default());
        let (unreachable, _) = relay(MIDDLE, RelayLimits::default());
        let (mut client, create) = CircuitClient::start(&guard).unwrap();
        net.round(&mut client, guard.peer_id(), create);
        let cell = client.extend(&unreachable).unwrap();
        let table = net.tables.get_mut(guard.peer_id()).unwrap();
        let out = table.handle(CLIENT, &cell, NOW).unwrap();
        let Outbound::Send { peer_id, msg } = &out[0] else { panic!("expected a send") };
        assert_eq!(peer_id, unreachable.peer_id());
        let failed = table.extend_unreachable(peer_id, msg.link()).unwrap();
        let Outbound::Send { msg, .. } = failed else { panic!("expected a send") };
        assert_eq!(client.handle(&msg).unwrap(), ClientEvent::ExtendFailed);
        assert!(table.extend_unreachable(unreachable.peer_id(), [0u8; 16]).is_err());
    }

    #[test]
    fn a_disconnected_peer_takes_its_circuits_with_it() {
        // The middle disappears: the guard tells the client, the exit drops.
        let (mut net, mut client, [guard, middle, exit]) = built(RelayLimits::default());
        let now = net.now;
        let out = net.tables.get_mut(guard.peer_id()).unwrap().peer_disconnected(middle.peer_id(), now);
        net.push(guard.peer_id(), out);
        let out = net.tables.get_mut(exit.peer_id()).unwrap().peer_disconnected(middle.peer_id(), now);
        assert!(out.is_empty(), "nothing to send to a peer that is gone");
        assert_eq!(net.drain_client(&mut client), vec![ClientEvent::Closed]);
        assert!(net.tables[guard.peer_id()].is_empty());
        assert!(net.tables[exit.peer_id()].is_empty());
        assert_eq!(net.tables[guard.peer_id()].circuits_from(CLIENT), 0);

        // The client disappears: every hop is told in turn.
        let (mut net, _client, [guard, _middle, _exit]) = built(RelayLimits::default());
        let now = net.now;
        let out = net.tables.get_mut(guard.peer_id()).unwrap().peer_disconnected(CLIENT, now);
        net.push(guard.peer_id(), out);
        net.run();
        assert!(net.tables.values().all(|t| t.is_empty()));
        assert!(net.to_client.is_empty());
    }

    #[test]
    fn a_rejected_cell_does_not_refresh_the_idle_timer() {
        let limits = RelayLimits { idle_timeout_ms: 1000, ..RelayLimits::default() };
        let (mut net, mut client, [guard, _middle, _exit]) = built(limits);
        let LinkMsg::Cell { link, seq, mut ciphertext } = client.seal_data(2, b"x").unwrap() else { unreachable!() };
        ciphertext[0] ^= 1;
        let forged = LinkMsg::Cell { link, seq, ciphertext };
        let table = net.tables.get_mut(guard.peer_id()).unwrap();
        assert!(table.handle(CLIENT, &forged, NOW + 900).is_err());
        // Garbage must not keep a circuit open.
        assert!(!table.expire(NOW + 1000).is_empty());
        assert!(table.is_empty());
    }
}
