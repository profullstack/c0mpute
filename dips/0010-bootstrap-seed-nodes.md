---
dip: 0010
title: "Operator-run seed nodes for libp2p Kad-DHT bootstrap"
status: Accepted
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-05-03
discussion:
implementation: deferred until c0mpute-net wires up libp2p
supersedes:
superseded-by:
---

## Summary

To join the c0mpute Kad-DHT, a new peer needs at least one already-known
peer. We provide that via **3–5 operator-run seed nodes** with stable
public addresses + known peer IDs. The list is hardcoded in the
`c0mpute` binary AND published as a JSON file at
`https://c0mpute.com/bootstrap.json` so we can update it without a
binary release.

Seed nodes only help discovery. They don't see job content (chunks are
content-addressed), can't forge receipts (those are signed by CoinPay
DIDs), and can't censor jobs (peers connect directly once discovered).
A malicious seed can deny new peers entry at worst — it can't
compromise integrity.

## Motivation

Every Kad-DHT needs an initial-contact mechanism. Without one, a new
peer has no way to find existing peers. Bitcoin uses DNS seeders
(`seed.bitcoin.sipa.be` etc.); IPFS / libp2p use a list of bootstrap
multiaddrs. Both work; we pick the libp2p convention because it's what
our stack already speaks.

The danger: if seeds are unreliable, the network can't onboard new
peers. That's why we run multiple, on different infrastructure, in
different regions.

## Detailed design

### Bootstrap list shape

The hardcoded list in the `c0mpute` binary plus the JSON feed both
share this shape:

```json
{
  "version": 1,
  "protocol_id": "/c0mpute/kad/1.0.0",
  "peers": [
    {
      "id": "12D3KooW...",                // libp2p peer id
      "addrs": [
        "/dns4/seed-iad.c0mpute.com/tcp/4001",
        "/dns4/seed-iad.c0mpute.com/udp/4001/quic-v1"
      ],
      "operator": "Profullstack",
      "region": "us-east-1"
    }
  ]
}
```

The CLI fetches `https://c0mpute.com/bootstrap.json` on startup, merges
with the hardcoded list, and uses the union for bootstrap. If the
fetch fails, it falls back to the hardcoded list. If both fail, it
exits with a clear error.

### Operating constraints for a seed node

A seed node is a c0mpute worker started in a constrained mode:

```
c0mpute worker start --roles storage,verifier --bootstrap
```

The `--bootstrap` flag:
- Advertises the node as a public seed (sets a capability tag).
- Disables transcode + inference roles regardless of `--roles`
  (seeds shouldn't compete for jobs).
- Prefers stable IP addresses (refuses to start if behind unstable
  NAT and no public address is configured).
- Rotates peer-id less frequently (no churn).

### Anchor topology for v1

Three to five seeds, run by Profullstack:

| Region | Provider | Why |
|---|---|---|
| us-east-1 | Hetzner / OVH | Stable IP; cheap; not in the AWS blast radius |
| us-west-2 | DigitalOcean | Different ASN / coast |
| eu-central-1 | Hetzner | Latency for European peers |
| ap-southeast | Hetzner / OVH | Asia-Pacific reach (P2 if budget allows) |
| (us-west, Railway) | Railway | Easy ops, but Railway containers don't have stable IPs by default — only as P2 |

We launch with the first three. Add the rest as load justifies.

### Custom protocol id

The DHT protocol id is `/c0mpute/kad/1.0.0`. This isolates us from the
public IPFS network (which uses `/ipfs/kad/1.0.0`). Random IPFS peers
won't accidentally end up in our DHT or vice versa.

Was `/quest/kad/1.0.0` in the original Quest PRD — renamed during the
c0mpute rebrand (DIP-0005).

### Trust boundary

Seeds are **discovery infrastructure, not authority**. Specifically:

- **Can do:** introduce one peer to another, publish their own
  capability advertisements.
- **Can NOT do:** see chunk contents (content-addressed), forge
  receipts (signed by CoinPay DIDs), reject jobs (workers and buyers
  connect directly once discovered), censor reputation (CoinPay
  arbitrates).

Worst-case: a malicious seed can refuse to forward DHT queries,
slowing peer discovery. The mitigation is having multiple seeds + the
JSON fallback list.

### Updating the seed list

Process:

1. Edit `bootstrap.json` in the c0mpute repo (under `web/public/` or
   wherever Railway serves it).
2. PR + merge.
3. Railway redeploys the apex; CDN cache flushes within minutes.
4. Existing nodes pick up the new list on their next periodic refresh
   (every 6h by default).

Hardcoded list updates require a binary release. They're a fallback
so we update them rarely — only when seeds rotate keys or move to new
hosts permanently.

### Configuration

In the user's `c0mpute` config:

```toml
[bootstrap]
# Override the JSON feed URL (defaults to https://c0mpute.com/bootstrap.json)
url = "..."

# Refuse to fall back to the hardcoded list (paranoid mode)
require_remote = false

# Add personal / community-run seeds
extra = [
  "/dns4/my-seed.example.com/tcp/4001/p2p/12D3KooW...",
]
```

## Alternatives considered

**DNS seeders (Bitcoin-style).** A DNS A record returns the IP of one
of N seed nodes round-robin. Works without HTTPS, very robust. But
loses the structured metadata (peer IDs, capabilities). Could co-exist
later as a redundant fallback.

**No seeds at all.** Not viable for a Kad-DHT. mDNS-only discovery
works on a LAN but not the internet.

**Decentralized seed discovery.** PEX (peer exchange), tracker DHTs,
etc. These work for steady-state but not for cold-start. We need at
least one entry point.

## Migration & rollout

This DIP only locks the design. Implementation depends on `c0mpute-net`
actually wiring up libp2p (currently a trait surface). Sequence:

1. `c0mpute-net` implements libp2p Kad-DHT with protocol id
   `/c0mpute/kad/1.0.0`.
2. Bootstrap-list parsing lands in `c0mpute-core` config.
3. CLI flag `--bootstrap` lands in `c0mpute worker start`.
4. We deploy 3 seed nodes on Hetzner / DO / Hetzner-EU.
5. Peer IDs + multiaddrs go into the hardcoded list AND
   `bootstrap.json`.
6. Public mainnet launch.

## Open questions

- **DNS vs static IP for multiaddrs.** DNS (`/dns4/seed-iad.c0mpute.com/...`)
  lets us swap servers without binary releases. Tradeoff: DNS resolution
  is one more failure mode at startup. Probably worth it.
- **TLS / Noise key rotation.** Seeds will eventually want to rotate
  their long-term identity keys. Rotation policy is its own DIP.
- **Community-run seeds.** Eventually we'd want to advertise a way for
  third-party operators to register their seeds in the JSON feed.
  Out of scope for v1 (we run the seeds; community joins later).

## Out of scope

- Anchor relay nodes for hole-punching (PRD §14 mentions QUIC + TCP
  fallback). Same operational model but a separate role.
- IPFS / Filecoin interop. We deliberately use a custom protocol id.
