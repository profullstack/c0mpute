# Scallion Network: threat model (v0.1, draft for review)

**Status:** draft. Written before the data plane exists, so the design can be
checked against it. It has not been reviewed by anyone outside the project.
Scallion is **not a working VPN** and makes no anonymity claim; see
[client modes](scallion-clients.md).

Version this document. A change to a goal, a non-goal or an adversary is a
new version, and the code that depends on it says which version it targets.

## What Scallion is for

A client reaches a destination (a website, an API, a shell, a c0mpute worker)
through three independently operated relays: a **guard**, a **middle** and an
opt-in **exit**. The purpose is that no single relay, and no single observer of
one link, can tell both who the client is and what they are talking to.

## Parties

| Party | Sees by design |
|---|---|
| Client | everything about its own circuit |
| Guard | the client's IP address, the middle's identity, timing and volume |
| Middle | the guard's identity, the exit's identity, timing and volume |
| Exit | the middle's identity, the destination, and the content unless the application encrypts it (TLS) |
| Destination | the exit's IP address and whatever the application sends |
| Directory / gossip | which relays exist and what they claim; never who uses them |

## Security goals

1. **Unlinkability against one relay.** A guard alone cannot learn the
   destination. An exit alone cannot learn the client. A middle alone learns
   neither.
2. **Relay authentication.** A client only builds a circuit through relays
   whose Noise key is signed by the libp2p identity it expects. An active
   attacker on the path cannot stand in for a relay.
3. **Forward secrecy.** Stealing a relay's long-lived key later does not
   decrypt traffic recorded earlier.
4. **Integrity and replay protection.** A relay or network attacker cannot
   alter, inject or replay cells without detection.
5. **Fail closed.** No path, a failed handshake or a dead circuit means no
   traffic. Never a direct connection, never a shorter path, never the other
   client mode.
6. **Exit consent.** A node carries exit traffic only if its operator opted
   in, and only to the ports and public addresses its policy allows
   ([exit policy](scallion-exit-policy.md)).
7. **Bounded cost.** A relay's memory, time and bandwidth per circuit and per
   peer are capped, so the network cannot be used to exhaust a relay.

## Not goals

- **A global passive adversary.** Someone who watches both the client's link
  and the exit's link can correlate timing and volume. Low-latency onion
  routing does not defeat this, and Scallion will not claim to.
- **Hiding that you use Scallion.** The guard link is recognisable.
- **Protecting plaintext.** The exit reads anything the application does not
  encrypt.
- **Anonymity from the destination's own identification.** Logging in
  identifies you.
- **Getting around legal requirements.** Sanctions, export controls and
  identity checks a service must perform are out of scope and must not be
  bypassed with this network.

## Adversaries and what answers them

| Adversary | Capability | Answer | State |
|---|---|---|---|
| Curious single relay | reads what it is given | layered encryption; telescoped build so each hop knows only its neighbours; per-link circuit ids | handshake implemented, not wired |
| Network attacker on a link | read, drop, inject, replay | Noise NK per hop, AEAD, explicit sequence numbers with a sliding window | implemented, not wired |
| Impersonator | answers in a relay's place | descriptor signed by the relay's libp2p identity; handshake bound to the peer id and the link id | implemented, not wired |
| Later key theft | steals a relay static key | ephemeral-ephemeral key agreement in every handshake | implemented, not wired |
| Colluding guard and exit | compare timing and volume | guard pinning limits how often a client meets a hostile guard; path selection requires distinct operators and ASNs | planner only; operator claims are unverified |
| Sybil operator | runs many relays under different labels | not solved. Declared operator and ASN are claims. Needs measured diversity, stake or reputation | open |
| Lying relay | advertises speed or roles it does not have | routes ranked from the client's own measurements, never self-report; exit role only via signed descriptor | planner implemented; measurement not wired |
| Malicious exit | reads or alters plaintext, attacks the destination | application TLS is the only defence for content; exit policy limits where an exit will connect | policy implemented, no exit service |
| Malicious client | uses relays to attack third parties or the relay | exit policy (ports, public addresses only, no rebinding), per-circuit and per-peer quotas, abuse handling | policy implemented; quotas open |
| Resource exhaustion | opens circuits, never finishes | bounded message sizes are enforced; circuit and handshake rate limits and timeouts are still needed | partly; limits open |
| Downgrade | forces an older or weaker protocol | one protocol version and one Noise pattern, named in the handshake prologue; no negotiation | implemented |
| Local leak | DNS, IPv6 or an app bypassing the tunnel | SOCKS5 with remote DNS first; TUN with a kill switch later; leak tests gate release | not implemented |
| Traffic analysis | cell sizes, counts and timing | not addressed yet. Cells are not padded to a fixed size and there is no cover traffic | open |

## Known weaknesses of the current handshake design

Stated here so a reviewer does not have to find them:

- **Cell size reveals hop position.** Each layer adds a fixed overhead, so a
  link observer can tell the guard link from the exit link by size. Fixed-size
  cells need a different layering and are a later version.
- **A relay knows it is forwarding.** It is told its next hop when asked to
  extend, and so learns whether it is the last hop.
- **One stream per circuit.** There is no stream multiplexing or flow control
  yet; the payload is opaque bytes.
- **No rekeying.** A circuit uses one set of keys for its life. Circuits must
  be short-lived until rekeying exists.
- **Link-level destroy is unauthenticated.** Any hop can tear a circuit down.
  A hop can always drop traffic anyway, so this is denial of service it
  already had.
- **Descriptor distribution is unspecified.** How a client fetches
  descriptors without revealing which relays it will use is not designed.
- **No revocation.** A stolen relay key is valid until its descriptor expires
  (at most 7 days).

## Release gates that follow from this

A mode may report `ready` only when every row above marked implemented is
wired to real connections and tested across independent hosts, the open rows
that matter for the claim being made are closed or the claim is narrowed to
match, and an independent party has reviewed this document and the code.
`c0mpute scallion doctor` reports the outstanding blockers.

See [handshake](scallion-handshake.md), [architecture](scallion-network.md),
[frames v1](scallion-frames.md) and issue #386.
