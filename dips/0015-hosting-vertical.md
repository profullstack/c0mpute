---
dip: 0015
title: "Hosting vertical: censorship-resistant static sites on continuous reservations"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-05-20
updated: 2026-05-20
discussion:
  - https://github.com/profullstack/c0mpute/discussions
implementation:
supersedes:
superseded-by:
---

## Summary

c0mpute offers a **censorship-resistant static hosting** vertical built
on top of the storage primitive from DIP-0012 v3 (Reed-Solomon 10/14
shards + auto-repair). The contract shape is different from the
transcode auction: a **continuous reservation** with multi-replica
placement, epoch-based settlement, proof-of-serve challenges, and
slashable collateral. The differentiator is **takedown resistance and
replica diversity as contract terms**, not price.

This is a narrow vertical. It does *not* attempt to compete with
Cloudflare or Vercel on latency or $/GB. DIP-0013's GPU-batch
positioning is unchanged; this DIP carves out one additional
non-competing vertical.

## Motivation

The compute marketplace + storage primitive together already cover
"upload a blob, get it back later." That isn't a website. A website
needs:

1. **Continuous availability** — not "complete the job and disappear."
2. **Public reads** by anonymous clients, not authenticated workers.
3. **Mutable naming** — `example.eth` resolves to whatever the owner
   most recently published, with a tamper-evident history.
4. **Takedown resistance** — replica diversity as a contractual
   guarantee, not a best-effort placement heuristic.
5. **Settlement that pays for uptime + egress**, not job completion.

None of those are well-served by extending the transcode auction. A
job-shaped contract assumes a discrete completion event; hosting has
no such event — it just keeps serving until the reservation expires.
Forcing hosting through the job-receipt path produces either constant
re-billing receipts (gas-style) or stale receipts that hide outages.

Hosting also has an immediate distinct customer ask: "I want my
content to survive a takedown order." Cloudflare and Vercel cannot
sell that — they're single-jurisdiction surfaces. This is the same
shape as DIP-0012 v3's argument for storage: we don't compete on
price, we compete on a value bundle incumbents structurally cannot
offer.

## Detailed design

### Site model

A "site" is a **manifest object** referencing many file objects:

```json
{
  "kind": "site_manifest",
  "version": 1,
  "entries": [
    { "path": "index.html",       "object_hash": "blake3:..." },
    { "path": "assets/app.js",    "object_hash": "blake3:..." },
    { "path": "assets/logo.svg",  "object_hash": "blake3:..." }
  ]
}
```

Each entry is itself an RS-encoded object in the storage primitive
(DIP-0012 v3). The manifest is also an object. The site root =
hash of the manifest.

Updating the site = upload changed files + new manifest + sign a
name-pointer update (see name layer below). Unchanged files dedup
in the content-addressed layer for free.

### Reservation contract

A host reservation replaces the one-shot offer/bid/accept/receipt
flow with a longer-lived contract.

```json
{
  "kind": "host_reservation_request",
  "site_root":            "blake3:...",
  "name":                 "example.eth",
  "duration_hours":       8760,
  "replicas_min":         14,
  "replicas_target":      21,
  "diversity": {
    "asn_min":     6,
    "country_min": 4
  },
  "price_caps": {
    "storage_per_gb_hour": "0.00001100",
    "egress_per_gb":       "0.005000"
  },
  "challenge_policy": {
    "epoch_seconds":         3600,
    "storage_per_epoch":     3,
    "serve_per_epoch":       1,
    "consecutive_fail_slash":3
  }
}
```

Workers bid per-shard (or per-group-of-shards):

```json
{
  "kind": "host_reservation_bid",
  "reservation_id":       "...",
  "shard_indices":        [3, 7],
  "storage_per_gb_hour":  "0.00001000",
  "egress_per_gb":        "0.004000",
  "collateral_pledge":    "coinpay:...",
  "asn":                  "AS12345",
  "country":              "DE"
}
```

The scheduler picks winners by `price × reputation × diversity_fit`,
where `diversity_fit` enforces the `asn_min` / `country_min` floors
across the accepted bid set. Existing DIP-0011 gossipsub auction
machinery is reused; the new bits are the diversity constraint solver
and the per-shard (rather than per-job) match.

### Settlement cadence: hourly epochs

Each `(worker, reservation)` pair produces one signed attestation per
epoch:

```json
{
  "kind": "host_epoch_attestation",
  "reservation_id":   "...",
  "worker_did":       "did:coinpay:...",
  "shard_indices":    [3, 7],
  "epoch_start":      "2026-05-20T13:00:00Z",
  "epoch_end":        "2026-05-20T14:00:00Z",
  "stored_bytes":     4823491200,
  "served_bytes":     142000000,
  "served_count":     38,
  "served_log_root":  "blake3:...",
  "sig":              "..."
}
```

`served_log_root` is the Merkle root of an append-only request log
the worker keeps for that reservation:

```
leaf = blake3( ts || object_hash || byte_offset || byte_length )
```

Per-request signing would be gas-prohibitive. Instead the worker
publishes only the root; validators audit by demanding random leaves
and the inclusion proofs. Same shape used by Filecoin retrieval and
Storj satellite billing — statistical, not exhaustive.

### Two challenge classes

Storage and hosting need different challenges. Both run per epoch.

**Storage challenge (already typed as `c0mpute-verify::StorageChallenge`):**
Validator picks `(shard_index, byte_offset, length)`. Worker must
return the bytes plus a proof they belong to the committed shard.
This proves the worker still *has* the data.

**Serve challenge (new):** Out-of-network probe issues an HTTPS GET
to the worker's public gateway endpoint, fetching the same range as
the storage challenge from the *reconstructed object* (not the raw
shard). The bytes must hash to the expected value. This proves the
worker is *reachable to the public internet*, which storage challenge
alone does not — a worker could pass storage challenges over the
internal libp2p mesh while being silently firewalled from public
browsers.

Serve challenges originate from a validator pool whose nodes
explicitly route over diverse network paths (multiple ASNs, ideally
some over Tor) to defeat trivial allowlisting.

### Payout formula

Per epoch, per `(worker, reservation)`:

```
payout = stored_bytes_avg / 1e9 × storage_per_gb_hour
       + served_bytes     / 1e9 × egress_per_gb
       - challenge_failures × slash_per_failure
```

Settled through CoinPay against the customer's escrowed reservation
budget (locked at reservation time, draws down per epoch).

### Stake and slashing

At bid acceptance, each worker locks collateral:

```
collateral = (storage_per_gb_hour × stored_gb + egress_per_gb × expected_gb_egress)
           × duration_hours
           × slash_multiplier
```

`slash_multiplier` starts at 2.0 (configurable per-reservation). The
stake is released linearly as attestations pass. Forfeitures:

- Failed storage challenge: 0.01% of remaining stake
- Failed serve challenge:   0.10% of remaining stake
- `consecutive_fail_slash` failures in a row: replica de-listed,
  remaining stake forfeit, Phase 4 auto-repair from DIP-0012 v3
  triggers re-placement to a new peer
- No attestation submitted for an epoch: counts as a failed serve
  challenge

This is what makes "can't be taken down" economically real. A worker
who voluntarily complies with a takedown order loses its stake; a
worker who is *forced* offline by infrastructure loses its stake;
either way the replica reconstitutes elsewhere.

### Name layer

Mutable sites need a `name → site_root` binding that:

- Resolves cheaply (browsers and gateways query it on every fetch)
- Has a tamper-evident history (so a forced-update can't quietly
  rewrite the past)
- Is owned by the customer's DID (CoinPay-anchored, per DIP-0007)

A name registry record:

```json
{
  "kind": "name_pointer",
  "name":             "example.eth",
  "owner_did":        "did:coinpay:...",
  "site_root":        "blake3:...",
  "prev_site_root":   "blake3:...",
  "ts":               "2026-05-20T13:00:00Z",
  "sig":              "..."
}
```

Records are appended to a CoinPay-anchored log keyed by `name`.
Resolvers (browsers, gateways) follow the latest signed record. The
history is queryable — anyone can produce the chain of updates to
prove a current resolution.

ENS interop is deferred — c0mpute names live in their own namespace
for v1; an ENS adapter is straightforward later.

### Gateway plurality

Any c0mpute node with the `gateway` role + `host` role can serve a
site over HTTPS:

```
GET https://{gateway}/c0mpute/host/{name}/{path}
GET https://{gateway}/c0mpute/host/blake3:{site_root}/{path}
```

c0mpute.com runs one canonical gateway. Anyone can run another from a
clone of this repo and serve the same content (the data lives in the
storage layer + name registry, not at any gateway we own). If the
canonical gateway is blocked, clients fall back to any peer gateway,
or — eventually — a browser extension that speaks `c0mpute://`
natively.

This is the same shape as the status-aggregator plurality argument
from DIP-0014: anyone can run one, no privileged operator.

### Diversity as a contract term

Phase 3 of DIP-0012 v3 plans for ASN / region diversity at *placement
time* as a durability heuristic. This DIP elevates diversity to a
*settlement-relevant* property:

- Workers self-declare `asn` and `country` in their bid (challengeable;
  the validator pool spot-checks geolocation against the worker's
  observed network path).
- The acceptance algorithm rejects a bid that would violate the
  reservation's `asn_min` or `country_min` floor.
- If a worker's declared jurisdiction is later contradicted by
  observation (e.g., its IP is consistently routed from a different
  country across multiple validator probes), the reservation can
  treat it as a failed serve challenge.

This is what turns "censorship-resistant" from a marketing claim into
a verifiable contract clause.

## Integration with existing primitives

| Existing | How this DIP uses it |
|---|---|
| DIP-0007 CoinPay DIDs | Owner identity, name registry signatures, stake/payout settlement |
| DIP-0011 gossipsub auction | Reservation requests + bids propagate on a new `c0mpute/host/v1` topic |
| DIP-0012 v3 storage | Each file + manifest is an RS 10/14 object; Phase 4 auto-repair reused on slash-driven re-placement |
| `c0mpute-verify::StorageChallenge` | Storage challenge already typed; serve challenge is the new sibling |
| DIP-0014 status aggregator | Network-level hosting stats roll up as `hosted_sites`, `hosted_bytes`, `serve_bandwidth_24h` (aggregate only — never per-site) |

## Reconciliation with DIP-0013

DIP-0013 explicitly excludes "always-on services" and "CDN / asset
delivery" from c0mpute's competitive scope. This DIP narrows that
exclusion rather than removing it:

- **Still excluded:** competing with Cloudflare / Vercel / Fastly on
  latency, dynamic application hosting, edge compute, CDN cost-per-GB
  at scale. Customers who want any of those should keep using the
  incumbents.
- **Added vertical:** static sites where takedown resistance and
  replica-diversity guarantees are the differentiator. This is a
  recognizably different ask — most customers don't need it; the ones
  who do can't get it from incumbents at any price.

This is the same v1→v3 pattern DIP-0012 followed: position narrowly
on a value angle incumbents structurally cannot serve, not on price.

## Alternatives considered

**Extend the transcode-style job auction.** Mints a receipt per
serve. Either gas-prohibitive (every request signed) or stale
(receipts on long intervals hide outages). Doesn't fit hosting's
shape.

**Lean on IPFS / Filecoin retrieval.** Hand off retrieval to an
existing network. The compute-locality advantage from DIP-0012 v3
disappears (Filecoin nodes aren't c0mpute workers), and the diversity
guarantees become "whatever the other network does," which is too
weak to sell as a contractual property.

**Centralized takedown-resistant gateway.** A single c0mpute-operated
HTTPS gateway in a friendly jurisdiction. Single point of failure /
takedown by definition. Defeats the value prop.

**Per-request micro-payments.** Worker signs each request, customer
pays per request. Gas costs dominate the payout for normal-sized
files. Sampled audit on a Merkle log is strictly cheaper at
equivalent assurance.

**Skip mutable naming; sites are immutable by hash.** Possible — the
hash-only mode survives as a sub-case (no `name` field in the
reservation). But most customers want `example.com`, not
`blake3:e3b0c44...`. A name layer is the difference between hosting
and a CDN-quality URL.

## Migration & rollout

Phase 1 — **Contract types + reservation auction.** Define the
JSON shapes above in `c0mpute-host` (new crate). Gossipsub topic
`c0mpute/host/v1` for reservation requests + bids. Acceptance
algorithm with diversity-fit constraint solver. No actual hosting
yet — reservations dead-letter into a stub.

Phase 2 — **Storage challenge wiring** (reuses DIP-0012 v3 Phase 5).
Per-epoch storage challenges firing against active reservations,
payouts streaming through CoinPay.

Phase 3 — **Serve challenge + validator probe pool.** Out-of-network
probes hitting the public gateway endpoint. Validator pool diversity
requirements (multi-ASN, optionally Tor egress).

Phase 4 — **Name registry.** CoinPay-anchored append-only log of
`name_pointer` records. Resolver library (`c0mpute-host::resolve`)
used by gateways and the browser extension.

Phase 5 — **Site manifest + multi-file objects.** Manifest format,
client tooling to publish a directory (`c0mpute host publish ./dist
--name example.eth`).

Phase 6 — **Browser extension** that speaks `c0mpute://` natively
and resolves through the name registry without going through a
canonical gateway. Deferred — gateway plurality is enough for v1
adoption.

Each phase is independently shippable. Phase 1+2 alone gives an
immutable hash-only hosting product that proves the settlement
model; mutable names (Phase 4) and the polished site UX (Phase 5+)
follow.

## Open questions

- **Abuse policy.** "Can't be taken down" forces a CSAM / abuse
  stance. Now drafted as DIP-0016 (Draft, targets v2). v1
  (Phases 1–3) can ship as a hash-only beta without public
  takedown-resistance marketing; DIP-0016 must be Accepted and
  implemented before the v2 surface (public marketing, name
  registry, browser extension) goes live.
- **Browser story.** Gateway-mediated HTTPS works on day one but
  centralizes resolution. Browser extension is the long-term answer;
  pinning down extension UX (does it transparently rewrite URLs?
  show a `c0mpute://` URL bar treatment?) is non-trivial.
- **Hot egress pricing.** $0.005/GB internet egress at 10 TB/mo of
  public viewership is $50/mo per site — fine for a static page,
  punishing for video. Tiered egress (cheaper above a threshold) is
  probably needed if video hosting ever becomes a target use case;
  not needed for the static-site MVP.
- **Stake source.** Workers need CoinPay balance to post collateral.
  How that interacts with cold-start (a new worker with no balance
  can't accept any host reservations) — punted to CoinPay-side
  design, possibly via a delegated-stake pool.
- **ENS / DNS interop.** Customers will want `example.com` over the
  c0mpute name registry. DNS CNAME to a gateway works for v1; deeper
  ENS / Handshake / DNSSEC integration is its own DIP later.

## Out of scope

- Dynamic application hosting (databases, server-side rendering,
  edge functions). The vertical is **static** sites only — anything
  needing live compute is back in DIP-0011 territory.
- Low-latency CDN-quality serving. We accept p2p path latency as a
  trade for diversity; if a customer needs ≤20ms TTFB they should
  use Cloudflare.
- Per-site analytics / observability surfaces. Out of bounds by the
  DIP-0014 privacy model.
- Storage of large binary blobs without a serving guarantee — that's
  the existing DIP-0012 v3 storage primitive, not this vertical.
- CSAM / abuse policy specifics — see DIP-0016 (targets v2; must
  land before public takedown-resistance marketing).
