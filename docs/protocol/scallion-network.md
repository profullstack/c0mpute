# Scallion Network: privacy-first P2P routing for c0mpute

> Scallion = green onion. Friendly name, serious privacy engineering. The network is decentralized and permissionless to join; some services may require independent compliance checks.

**Status:** experimental, route planner only; **no anonymity or VPN service is implemented by this change**.

This is a general-purpose privacy architecture for legally permitted browsing, APIs,
messaging, terminal access and compute services. The route planner in
`c0mpute-net::scallion` is a **reference implementation**, not a
cryptographic anonymity guarantee. Existing libp2p circuit relays are used to
reach NATed peers. They are **not** Tor-style onion circuits: a libp2p relay
can know its two connection endpoints. Do not present them as equivalent.

## Design

- **Admission:** cryptographic peer identities generated locally, no registration.
  No universal KYC requirement to join. Independent services or payment rails may
  require further verification where the law or their policies require it.
- **Transport target:** independently operated entry (guard), middle and exit
  nodes connected by authenticated, layered-encrypted circuits. Encrypted
  forwarding, circuit establishment, an exit service and a local SOCKS5/TUN
  front-end **are not implemented by this PR**.
- **Routing:** users pin a stable entry guard rather than changing entry
  identity on every rotation. Select independently owned middle and exit
  nodes. Require an explicit opt-in for exit-node operation. Select using
  recent observed link RTT, loss and available capacity, not provider
  self-reported speed.
- **Privacy:** no identity, location or destination is placed in a public
  peer advert for this feature. Network peers may still learn source IPs,
  timing and traffic volume; KYC-free and DID-based do not equal anonymous.
- **Session stickiness:** established sessions keep their circuits.
  Rotate only **new** streams after a lease expires. When an old session
  finishes, its old circuit drains. This prevents avoidable breakage of
  SSH/HQTerm, inference streaming and WebSockets.
- **Exit safety:** an operator explicitly opts in to exit duty, determines
  allowed ports/destinations and handles abuse. Do not automatically turn
  existing GPU workers into general-purpose internet exits.
- **Regulated services:** routing must not be used to defeat legally required
  sanctions screening, export controls or identity verification. A VPN/proxy
  cannot make a GPU transaction legally permissible.

## Path scoring: fast without the insecure "fastest peer always" rule

The Scallion `rank_routes` function:

1. Requires a pinned and currently eligible guard.
2. Requires three distinct peer identities, independently asserted
   operator/family identifiers and different non-zero ASNs.
3. Requires all three link measurements, each within its maximum age, and
   positive bandwidth for every hop.
4. Rejects exits that have not explicitly opted in.
5. Estimates the path cost from summed sampled RTT, a loss penalty and
   transfer time at the path's bottleneck link.
6. Sorts deterministically; `choose_near_fastest` samples from paths
   within a configured performance margin so the cheapest path does not
   absorb all traffic.

**Important:** declaring different ASNs or operator IDs is not proof that
peers are independent; ASN alone also does not prevent a shared network
observer. Authenticity, measurement fraud and path-family verification
need separate engineering and a security audit.

Continuous aggressive network-wide probing can itself undermine privacy,
cause detectable patterns or consume volunteer bandwidth. A production
system should sample sparingly, avoid publishing client probes publicly
and account for probe-induced anonymity risk.

## Session continuity

A Scallion `CircuitLeases` instance assigns a stable circuit ID to each established
application session. Changing the current route does not move active
sessions. The application may reconnect to an existing server-side
session after a connection break; that's different from magically
preserving a TCP socket when the exit IP changes.

- **HQTerm / HQSH:** `hqterm` is the TUI/CLI; `hqsh` is its persistent
  remote-shell service. A disconnection may be followed by reattachment
  to the same shell process; the transport itself is still SSH-based.
  See https://github.com/profullstack/hqterm.
- **Mosh:** supports roaming of its own UDP terminal session. It is not
  a VPN or a generic stream multiplexer for HTTP/SSH/other apps.
- **Ordinary TCP:** keep the old circuit until stream completion; use
  application-defined reconnect + resumable state for failure recovery.
- **GPU jobs:** the P2P workload owner must track a stable job/session ID;
  if the transport changes, reconnect to the same provider/session,
  with idempotent operation and authenticated resume tokens.

## Existing libp2p bootstrap behavior and limitation

Core nodes currently use
`bootstrap::fetch_or_fallback(DEFAULT_BOOTSTRAP_URL, default_seeds())`,
which performs an HTTPS request to c0mpute.com and otherwise uses
hardcoded seeds. Those seed operators can see a startup request, and
using their seeds is not equivalent to joining through independent
community contacts. For privacy-sensitive usage, a no-default-HTTP
bootstrap option and independent bootstrap peers are required. This PR
does not yet change that behavior.

## Test plan

`cargo test -p c0mpute-net privacy_routes` (when Rust dependencies are
available) covers independent role selection, exit consent, stale
measurement rejection, alternative-route selection, no direct fallback
and preserving established sessions during rotation. These are **unit
tests of route decisions**, not cryptographic or multi-node E2E tests.

### Follow-up integration work (not done here)

1. Specify a threat model: traffic correlation, hostile guards/exits,
   compromised nodes, disclosure of DNS/SNI, guard fingerprinting, exit
   abuse, malicious route metrics and downgrade paths.
2. Security review before building onion packet encryption; prefer
   reviewed cryptographic primitives and externally auditable protocol.
3. Implement three-hop circuit establishment, replay resistance,
   forward secrecy, key rotation, per-hop ratcheting and padded data
   transport; test that middle hops cannot see source and destination.
4. Build explicit exit-role consent and destination policy. Start with
   opt-in HTTP CONNECT/SOCKS5, then evaluate TUN support; enforce no
   direct-network leak fallback.
5. Integrate route metrics and leases with actual libp2p connections,
   authenticated link samples and client-controlled guard selection.
6. Add full multi-node and failure-injection tests for streamed APIs,
   persistent shells, GPU inference and legal-compliance boundaries.
7. Conduct an independent security audit before marketing privacy claims.

## Performance acceptance targets

Do not assert superiority to Tor without measurements. Measure setup
latency, throughput at different payload sizes, first-token time,
reconnect time, long-lived stream interruption and overhead versus
direct links and established anonymity networks. Prefer a stable guard
and diverse peers over a trivial global-minimum latency route.

## Naming and visual identity

- **Name:** Scallion Network
- **Tagline:** *Green routes. Private roots.*
- **Style:** scallion green, healthy and approachable; avoid imagery or claims implying that this is Tor or a production-ready anonymous VPN.
- **Code namespace:** `c0mpute_net::scallion` (experimental route planner only).
- **Future CLI surface:** `c0mpute scallion …`, not implemented yet.
