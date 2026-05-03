---
dip: 0012
title: "c0mpute hosts files: Reed-Solomon 10/14, compute-locality value"
status: Accepted
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-05-03
discussion:
implementation: c0mpute-store::erasure (shipped) + storage role on workers + HTTP shard endpoints (in progress)
supersedes:
superseded-by:
---

## Summary

c0mpute **hosts files**. Workers that opt into the storage role accept
Reed-Solomon-encoded shards (k=10, n=14) and serve them over HTTP.
Auto-repair re-shards on node failure.

We do **not** position this as "cheaper than R2". We position it as:

1. **Compute-locality** — data lives where the work happens; reading
   from c0mpute storage into a c0mpute worker is internal-egress-free.
2. **Sovereignty / E2E encryption** — customer-held keys; the network
   sees only ciphertext.
3. **No vendor lock-in** — content-addressed, portable across workers.
4. **Pay-as-you-go** — no minimums, no 90-day floors.
5. **Idle-disk arbitrage** — same shape as the GPU arbitrage; workers
   have spare disk on prosumer rigs.

## Motivation

This DIP went through three drafts; recording the path so the next
person doesn't repeat it.

| Draft | Position | Why withdrawn / kept |
|---|---|---|
| v1 | "no storage network" | Too narrow — storage really is part of the cloud-hosting story we want to tell |
| v2 | "build it, beat S3 on $/GB" | Withdrawn — couldn't survive the pricing math; per-GB consumer-network economics don't beat hyperscaler infrastructure spend |
| **v3 (this)** | **"build it, compete on value not price"** | Reconciles the two — yes we host, but we don't claim to beat R2 on raw $/GB |

The decisive directive from review: *"c0mpute will host files."*

The honest math from v2's withdrawal still stands: we won't be
cheaper than R2 ($0.015/GB + $0 egress) or B2 ($0.006/GB) on raw
$/GB-month. But we can offer a different bundle of values that some
customers will pay the same or slightly more for.

## Detailed design

### Pricing target (be honest)

| Provider | Storage $/GB-mo | Egress $/GB | Notes |
|---|---|---|---|
| Cloudflare R2 | $0.015 | $0 | The ergonomic winner for most |
| Backblaze B2 | $0.006 | $0.01 | The price winner |
| Storj | $0.004 | $0.007 | Centralized metadata; competitive |
| **c0mpute** | **$0.008** | **$0 internal**, $0.005 internet | Roughly B2 + a touch; internal egress is the value |

Crucial: customers who run c0mpute jobs that *read* data we host pay
**zero internal egress**. For transcode/inference workloads where the
input file dominates bandwidth, that's a real saving compared to
"store on R2, run inference on c0mpute" — even though R2 is also
$0-egress, the data still has to leave R2's network into the worker.
Same logical cost, but with c0mpute we keep it on-network.

### Architecture (single daemon, HTTP-shaped)

The c0mpute Rust daemon already runs an axum gateway. We extend it
with shard endpoints — same daemon, same auth, same operator
experience. Workers opt into the `storage` role just like they opt
into `transcode` or `gateway`.

```
┌────────────────────── c0mpute (Rust binary) ──────────────────────┐
│ axum gateway                                                       │
│   GET  /chunks/<hash>          (existing, content-addressed)       │
│   GET  /storage/v1/objects/<oh>     (NEW: erasure-coded read)      │
│   PUT  /storage/v1/objects/<oh>     (NEW: erasure-coded write)     │
│   GET  /storage/v1/shards/<sh>      (NEW: per-shard read)          │
│   PUT  /storage/v1/shards/<sh>      (NEW: peer placement target)   │
│   POST /storage/v1/repair/<oh>      (NEW: auto-repair trigger)     │
│ storage role  →  c0mpute-store + erasure-coding (shipped Phase 1)  │
└────────────────────────────────────────────────────────────────────┘
```

### Erasure-coding scheme

Reed-Solomon **10/14** (10 data + 4 parity). Already implemented in
`c0mpute-store::erasure` (DIP-0012 Phase 1 ships in tree as of
commit `6b5b771`):

- 40% storage overhead vs. 200% for 3-copy replication
- Tolerates 4 simultaneous shard losses
- Real-world durability ~11 nines under typical churn assumptions

Standard parameters; same Storj uses, well-trodden territory.

### Tiers

```
"storage": {
  "tier": "verified",  // "cheap" | "verified" | "private"
  "object_hash": "blake3:..."
}
```

| Tier | Scheme | Encryption | Use case |
|---|---|---|---|
| `cheap` | 3-copy replication | optional | Low-stakes / replaceable / dev |
| `verified` | RS 10/14 | server-side at-rest (worker-local key) | Default production |
| `private` | RS 10/14 | customer-encrypted before PUT (E2E) | Sovereignty / regulated content |

`private` is the genuine differentiator — workers process / serve
opaque ciphertext. If the workload is "store and serve later" without
any worker-side processing of plaintext, E2E is straightforward.
If the workload is "transcode this video", `private` doesn't apply
because the worker needs plaintext.

### HTTP wire format

Object PUT (multipart not required; raw body works):

```
PUT /storage/v1/objects/<expected-hash>
X-Coinpay-Auth: base64url(envelope)
Content-Type: application/octet-stream
Content-Length: <bytes>

<raw bytes>

→ 201 Created
{
  "object_hash": "blake3:...",
  "shards": [
    { "index": 0, "hash": "blake3:...", "host_hint": "12D3Koo..." },
    ...
    { "index": 13, "hash": "blake3:...", "host_hint": "12D3Koo..." }
  ],
  "tier": "verified"
}
```

Object GET:

```
GET /storage/v1/objects/<object-hash>

→ 200 OK
Content-Type: application/octet-stream
<reconstructed bytes>
```

Per-shard endpoints (for peer placement and auto-repair):

```
PUT /storage/v1/shards/<shard-hash>
X-C0mpute-Object: <object-hash>
X-C0mpute-Shard-Index: 0..13
<raw shard bytes>

GET /storage/v1/shards/<shard-hash>
→ raw shard bytes
```

Auth on PUTs: signed-request envelope keyed by CoinPay DID
(DIP-0007). Reads of public objects don't require auth; reads of
`private` tier do.

### Phase plan

**Phase 1 — DONE** (commit `6b5b771`):
- `c0mpute-store::erasure` — RS encode/decode (4 unit tests).
- `c0mpute-store::storage::Storage` — wrapper over `ChunkStore`,
  manifests on disk, single-node round-trip works (5 tests).

**Phase 2 — HTTP endpoints + manifest publishing:**
- Extend `c0mpute-gateway` with the shard / object endpoints above.
- Manifest publishing: when a worker stores an object, the manifest
  ends up in the customer's manifest registry (CoinPay-anchored,
  see Open questions).
- Single-node first; `host_hint` stays `null`.

**Phase 3 — Cross-node placement (waits on `c0mpute-net` libp2p):**
- Choose 14 distinct peers weighted by reputation × ASN/region
  diversity.
- PUT each shard to its assigned peer.
- Update manifest with `host_hint` for each shard.

**Phase 4 — Auto-repair daemon:**
- Periodic scan: for each manifest in our keep list, reach the 14
  shard hosts. If >2 unreachable, trigger repair.
- Repair: fetch 10 surviving shards → reconstruct → re-encode the
  missing 4 → PUT replacements to fresh peers → update manifest →
  sign a "repair completed" attestation through CoinPay.

**Phase 5 — Storage challenges + billing:**
- Random byte-range challenges per `c0mpute-verify::StorageChallenge`
  (already typed, just needs the dispatch).
- Payouts through CoinPay: $/GB-month accrued per shard host,
  $/GB egress accrued per server-of-bytes.

### What we keep from the BYOS3 idea (DIP-0013)

DIP-0013 still applies. **BYOS3 is the default; c0mpute storage is
opt-in.** Customers who already have R2 / B2 / S3 just keep using
those for free; we don't push storage on them. Customers who want
the compute-locality / sovereignty benefits opt into c0mpute storage
explicitly via job manifest.

## Alternatives considered

**Skip storage (v1 of this DIP).** Withdrawn — the cloud-hosting
story includes storage; pretending otherwise leaves a real gap.

**Try to beat R2 on raw $/GB (v2 of this DIP).** Withdrawn — the
math doesn't survive scrutiny, hyperscaler infra basis wins.

**This (v3) — build storage, sell it on different value props.**
Honest about positioning; keeps the compute-locality story which
is the actual structural advantage.

**Wrap Storj / Filecoin instead.** Their networks, their economics,
their tokens. We'd be a thin shim adding nothing. The compute-locality
value goes away because Storj/Filecoin nodes aren't c0mpute workers.

## Migration & rollout

The Phase 1 code shipped in `c0mpute-store` is already in tree.
Phase 2 adds the HTTP endpoints to `c0mpute-gateway` (in-progress
follow-up to this DIP). Phase 3+ depend on `c0mpute-net` (libp2p)
landing.

Marketing / pricing page on c0mpute.com should reflect this honestly:
storage is a c0mpute capability, $0.008/GB-month, with $0 internal
egress as the differentiator. **No claim of "cheaper than R2."**

## Open questions

- **Manifest hosting durability.** A manifest = a small JSON saying
  "these 14 shards on these 14 hosts make object X." If the manifest
  is lost, the data is unrecoverable even though shards exist.
  Probably the manifest itself becomes recursively a tier-`verified`
  object; the customer's keep-list is anchored at a CoinPay-signed
  registry pointer. Needs CoinPay integration to nail down.
- **Refcounting for shared shards.** Identical shards across objects
  share storage in the content-addressed layer (real dedup benefit).
  Deletion needs refcounting so we don't reap a shard another object
  still references. Phase 2 work.
- **Egress accounting.** Workers need to track "served N bytes for
  this object" to attribute payouts. Same plumbing as job-completion
  receipts.
- **Object size cap.** What's the largest single object we accept?
  Naive RS encoding loads full plaintext into memory at PUT time.
  Above some threshold (10 GB?) we need streaming RS encoding —
  doable, but extra work.

## Out of scope

- Filesystem-style mutable objects. Content-addressed, immutable.
- Permanent / Arweave-style storage.
- IPFS interop — could add later as a read-only adapter.
