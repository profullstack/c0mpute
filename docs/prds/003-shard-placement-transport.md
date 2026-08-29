---
cip: 003
title: "Cross-node shard placement and streaming transport"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-08-29
updated: 2026-08-29
implements: DIP-0012 (0012-storage-plugin.md) Phase 3
depends-on: 002
blocks: 005, 006
implementation:
estimate: "3–4 weeks"
---

## Summary

Spread each block's `n` shards across `n` distinct peers chosen for reputation
and failure-domain diversity, and make the libp2p transport capable of moving
them without loading whole shards into memory. This is the phase where
"distributed" stops being aspirational.

## Motivation

After CIP-002 the storage service works and stores every shard on one disk,
which provides no durability at all — one disk failure loses everything, and
the RS coding is pure overhead. `ShardEntry::host_hint` exists in the manifest
and is always `None`.

The network layer is further along than it looks. `c0mpute-net` already has an
846-line libp2p swarm with Kademlia, gossipsub, mDNS, and a request-response
protocol at `/c0mpute/chunk-fetch/1.0.0` that is already keyed by hash:

```rust
struct FetchRequest  { chunk_hash: Hash }
enum   FetchResponse { Ok { bytes: Vec<u8> }, NotFound }
```

That is exactly the right shape and exactly the wrong encoding, which is the
first thing this CIP fixes.

## Goals

- Select `n` peers per block, diverse by ASN and region, gated on reputation.
- Push shards to peers and fetch them back, streaming, with bounded memory.
- Populate `host_hint` and make reads fetch from peers.
- Degrade sanely: a read succeeds while up to `parity` hosts are unreachable.
- Publish and discover "who holds shard X" without a central index.

## Non-goals

- Repairing what's lost (CIP-005) — this phase detects and tolerates loss,
  it does not fix it.
- Paying anyone (CIP-006).
- Geographic *pinning* or data-residency guarantees. Diversity is for
  durability here, not for compliance.

## Design

### Fix the transport first

`request_response::cbor::Behaviour<FetchRequest, FetchResponse>` buffers an
entire response in a `Vec<u8>` before delivering it. With 4 MiB blocks and
RS 10/14 a shard is ~400 KiB, which is survivable; but the `hot` tier stores
whole 4 MiB blocks per replica, and `critical` reads fan out to 20 peers at
once. A node serving 50 concurrent reads would hold hundreds of MB in CBOR
buffers on a rig that is also running inference.

Replace it with a **streamed protocol** at `/c0mpute/shard/1.0.0`, using
`libp2p-request-response` with a custom `Codec` that reads and writes framed
chunks straight to and from disk:

```rust
enum ShardRequest {
    Get { shard_hash: Hash },
    Put { shard_hash: Hash, object_hash: Hash, block: u32, index: u8, len: u32 },
    Have { shard_hashes: Vec<Hash> },   // batched existence probe
}
```

Bodies stream in 64 KiB frames. The receiver hashes as it goes and rejects a
`Put` whose bytes don't match the declared `shard_hash` — same
commit-then-verify property as CIP-002's object PUT.

Keep `/c0mpute/chunk-fetch/1.0.0` registered and working for one release so
older nodes interoperate, then drop it. Protocol IDs are a public surface
(DIP-0003 territory), so the version bump is deliberate.

### Peer selection

Given a block needing `n` hosts, score each candidate peer:

```
score = reputation                        # c0mpute-verify::reputation, >= 0.9 required
      * uptime_30d                        # >= 0.99 required (CIP-001)
      * free_disk_factor                  # committed - used, normalised
      * (1 / (1 + rtt_ms / 100))          # prefer near peers, weakly
```

Then select greedily under **diversity constraints**, in priority order:

1. No two shards of the same block on the same peer. (Hard.)
2. At most `floor(parity / 2)` shards per ASN — 2 of 14 for `standard`. (Hard.)
3. At most `floor(parity / 2)` shards per region. (Hard.)
4. Prefer peers not already holding shards of the same *object*. (Soft.)

Constraint 2 is the one that matters and the one that will fail first. The
independence assumption behind CIP-001's durability table is worth nothing if
ten shards sit behind one residential ISP in one metro. If the network cannot
satisfy the constraints, **placement fails loudly rather than silently
degrading**: return 507 with which constraint could not be met. A small network
genuinely cannot store data durably, and pretending otherwise is how people
lose files.

Bootstrap reality: with fewer than ~30 storage nodes across ~5 ASNs,
`standard` placement will legitimately fail. Until then the network runs in
`hot` (n=3) which needs only 3 diverse peers, and the CLI says so plainly.

### Discovery: who holds this shard?

Two mechanisms, belt and braces:

- **The manifest is the primary index.** `host_hint` names the peer per shard.
  Reads go straight there. This is fast and needs no lookup.
- **Kad DHT is the fallback.** Providers announce `provide(shard_hash)` on the
  existing Kademlia behaviour. When a `host_hint` is stale — the peer moved,
  or repair relocated the shard — the reader falls back to
  `kad_find_node`/`get_providers`. `Swarm::kad_find_node` already exists.

Hints go stale constantly and that is fine; they are hints. The DHT is the
source of truth, the manifest is the cache.

### Read path

To read block `i`:

1. Take the `n` `host_hint`s from the manifest.
2. Fire `Get` to all `n` concurrently. Accept the first `k` that return.
3. Cancel the stragglers. RS decode. Verify against the block hash.
4. If fewer than `k` return within the deadline, resolve missing shards via the
   DHT and retry once.
5. If still short, return an error naming the block and how many shards were
   found — and enqueue a repair (CIP-005).

Requesting all `n` and taking the first `k` costs `n/k` = 1.4x read bandwidth
in exchange for cutting tail latency to the `k`-th fastest peer instead of the
slowest of a chosen `k`. On a network of consumer nodes with 200–500 ms
latencies that trade is clearly worth it. `critical` at 32 shards makes it
worse (1.6x), which is another argument for the 16/24 option in CIP-001's open
questions.

### Write path

1. Encode the block into `n` shards locally (already implemented).
2. Select `n` peers.
3. `Put` all `n` concurrently, with a deadline.
4. **Acknowledge the write once `k + ceil(parity/2)` shards are confirmed** —
   12 of 14 for `standard`. Full `n` placement continues in the background.
   This bounds write latency by the 12th-fastest peer rather than the slowest,
   while still leaving the object readable if the two stragglers never land.
5. Record `host_hint` for every confirmed shard; hand unconfirmed ones to the
   repair queue.

The write is durable at step 4 in the sense that the data survives `parity/2`
further failures. That is the honest definition and it is what `fsync` will map
onto in CIP-008.

## Acceptance criteria

1. A 5-node local testnet (mDNS, already supported) stores an object and each
   node holds strictly fewer than `k` shards of any block — verified by
   inspecting each node's chunk store.
2. Killing any 4 of 14 shard hosts still serves the object; killing 5 fails
   with an error naming the block.
3. Placement on a 3-node network with `standard` returns 507 naming the
   unsatisfiable diversity constraint, and does not write a partial object.
4. Streaming a 1 GiB object across the testnet keeps every node's RSS under
   300 MB.
5. A stale `host_hint` (peer restarted with a new address) still resolves via
   the DHT, with a tracing event recording the fallback.
6. Write acknowledges after 12 of 14 shards on a network where 2 peers are
   artificially delayed by 10s, and the remaining 2 land afterwards.
7. Old nodes speaking `/c0mpute/chunk-fetch/1.0.0` can still fetch chunks.

## Risks

- **Not enough diverse peers at launch.** The most likely blocker, and it's a
  supply problem, not a code problem. *Mitigation:* `hot` tier at n=3 works on
  a tiny network; operator-run seed nodes (DIP-0010) provide initial ASN
  diversity; the CLI reports how far the network is from supporting `standard`.
- **ASN lookup needs a data source.** A bundled IP-to-ASN table goes stale;
  a lookup service is a central dependency, which DIP-0011 forbids.
  *Mitigation:* ship an embedded table refreshed per release, degrade to /16
  prefix diversity when unknown. Prefix diversity is weaker but never wrong.
- **NAT.** Consumer nodes are behind NAT and libp2p hole-punching is not
  configured in the current swarm. Without it, a large fraction of "diverse"
  peers are simply unreachable. *Mitigation:* enable DCUtR + relay in the
  swarm as part of this CIP; treat relay-only peers as lower-scored, since
  relayed bandwidth is somebody else's cost.
- **Sybil placement.** One operator running 14 nodes across 14 VPS providers
  defeats diversity while satisfying every constraint. *Mitigation:* out of
  scope here; CIP-006's challenge economics and reputation are the lever, and
  this is an accepted residual risk for v1.

## Estimate

**3–4 weeks.** ~1 week for the streaming protocol and codec, 1 week for
selection and diversity constraints, 0.5 week for DHT provider records, 1 week
for the read/write paths with partial-failure handling, 0.5 week for the
testnet harness.

## Open questions

- Should `Have` batching be a separate protocol or folded into Kad provider
  records? Batched probes are much cheaper for CIP-005's repair scans.
- Relay-assisted peers: count them toward diversity or not? They are reachable
  but their bandwidth is a third party's.
- Does write-ack at `k + parity/2` need to be tier-configurable? `critical`
  users may want full `n` before ack.
