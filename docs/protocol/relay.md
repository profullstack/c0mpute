# Relays and NAT traversal

**Kind:** reference implementation, on by default on every node.
Relays are an accelerator: two peers that can dial each other never use
one, and no relay is required for any transaction.

Most c0mpute nodes sit behind a home router or a cloud firewall and listen
on a random port. Before this, a buyer could reach such a worker only if
the worker dialed out first (the dial-back in
`c0mpute-core/src/reachability.rs`). Now every node runs three standard
libp2p protocols, so any node can be dialed by peer id.

| protocol | libp2p id | what it does |
|---|---|---|
| AutoNAT v1 | `/libp2p/autonat/1.0.0` | asks peers to dial us back; confirms our public address, or concludes we are private |
| circuit relay v2 | `/libp2p/circuit/relay/0.2.0/hop`, `/stop` | a public node forwards connections for a private one |
| DCUtR | `/libp2p/dcutr` | over a relayed connection, both sides hole-punch to a direct one |

## What a node does

**Every node is a relay server.** A reservation it grants carries its
confirmed public addresses (from AutoNAT, or from `C0MPUTE_P2P_EXTERNAL`,
which a gateway with a fixed open port sets and which is confirmed at
start). A node with no confirmed external address refuses reservations
outright (a reservation rate limiter gated on it, since v0.3.59): before
that, nodes behind NAT or Docker granted empty reservations that clients
then rejected with `NoAddressesInReservation`. In practice the public
gateways and cloud workers carry the relays.

Server limits (raised from libp2p's defaults, which fit hole-punch
coordination only, so a job result can cross a relay when punching
fails):

| limit | value |
|---|---|
| reservations held | 256 (4 per peer) |
| reservation lifetime | 1 h (renewed by the client) |
| concurrent circuits | 64 (8 per peer) |
| circuit duration | 10 min |
| bytes per circuit | 32 MiB |

**Every node is a relay client.** When identify shows a connected peer
offering the hop protocol with a public TCP address, and AutoNAT has not
confirmed us public, the node listens on
`<relay addr>/p2p/<relay>/p2p-circuit`, which asks the relay for a
reservation. It keeps at most two reservations, so losing one relay never
makes it unreachable. A relay that refuses (or later drops) a reservation
is left alone for 10 minutes, or 6 hours when it answered with no
addresses. When AutoNAT later confirms the node public, it gives its
reservations back.

Public is sticky for 30 minutes: after AutoNAT said Public, a Private
verdict starts relay reservations only once that grace has passed (one
probe server that cannot dial back, being behind NAT itself, used to flip
public nodes). A node with `C0MPUTE_P2P_EXTERNAL` set is always public.

The accepted reservation shows up as a listen address,

```
/ip4/<relay ip>/tcp/<port>/p2p/<relay id>/p2p-circuit/p2p/<our id>
```

which identify and Kad hand to peers like any other address. A peer that
dials us by id therefore reaches us through the relay, and DCUtR then
tries to replace the relayed connection with a direct one.

## Opting out

`C0MPUTE_DISABLE_RELAY=1` stops a node from *reserving* relay slots (tests
that want only direct connections). It still serves as a relay, punches
holes and probes with AutoNAT. `C0MPUTE_RELAY_ALLOW_PRIVATE=1` lets a node
reserve on a relay with only private addresses; it exists for tests on one
machine and is useless on a real network.

## Where it lives

`node/crates/c0mpute-net/src/swarm.rs`: the `relay`, `relay_client`,
`dcutr` and `autonat` behaviours; `maybe_reserve_relay`;
`is_public_addr`, `plain_tcp_addr`, `wants_relay_reservation`. The test
`unreachable_node_is_reached_through_a_relay` runs a relay, a node that
listens nowhere, and a client that reaches it over the circuit with an
RPC.
