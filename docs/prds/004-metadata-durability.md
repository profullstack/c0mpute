---
cip: 004
title: "Metadata durability: manifests, volumes, and the root pointer"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-08-29
updated: 2026-08-29
implements: DIP-0012 (0012-storage-plugin.md) — resolves its "manifest hosting durability" open question
depends-on: 002
blocks: 005, 006, 007, 012
implementation:
estimate: "3–4 weeks"
---

## Summary

Give manifests somewhere durable to live, and give a customer a single stable
name for their data that survives every write. This solves the problem DIP-0012
flagged and left open:

> A manifest = a small JSON saying "these 14 shards on these 14 hosts make
> object X." If the manifest is lost, the data is unrecoverable even though
> shards exist.

The answer is a **volume**: a named, mutable root pointer, signed by a CoinPay
DID, whose value is the hash of an immutable metadata snapshot. Everything
mutable in the entire system reduces to advancing that one pointer.

## Motivation

Right now a manifest is a JSON file at `manifests/<hash>.json` on whichever
node happened to run the PUT. That node is a single point of total data loss,
and there is no way to enumerate "my objects" at all.

This is also the pivot the read/write filesystem depends on. Content is
immutable and content-addressed; a filesystem is mutable and path-addressed.
The only way to build the second from the first is a mutable pointer that names
an immutable tree. Get this layer right and CIP-007 is mostly bookkeeping. Get
it wrong and no amount of FUSE work will save it.

## Goals

- No single node's loss can orphan reachable data.
- A customer has a stable identifier for a mutable dataset.
- Root updates are atomic, ordered, and attributable to a DID.
- Recovery from a total client loss needs only the DID key.
- Bounded metadata cost for large datasets.

## Non-goals

- Concurrent multi-writer resolution (CIP-010 — this CIP assumes one writer
  at a time and detects, but does not merge, conflicts).
- POSIX semantics: inodes, permissions, directory entries (CIP-007).
- Encryption of metadata (CIP-011).

## Design

### Three layers

```
  Root pointer   volume id -> snapshot hash        mutable, signed, tiny
       │                                            (CoinPay-anchored)
       ▼
  Snapshot       an immutable metadata tree        content-addressed
       │         (object index / later: inodes)     stored as a `hot` object
       ▼
  Manifests      object hash -> blocks -> shards   content-addressed
       │                                            stored as `hot` objects
       ▼
  Shards         the actual bytes                  RS-coded across n peers
```

Every layer below the root is immutable and content-addressed, so it inherits
the durability of CIP-003's placement for free. **Only the root is mutable, and
it is 32 bytes.** That is the whole trick: concentrate all mutability into one
tiny signed value, then make that one value durable by other means.

### Manifests are objects

A manifest is stored via the ordinary object path at the `hot` tier
(3-copy — small, hot, cheap to repair; see CIP-001). It gets a manifest of its
own, which would recurse forever, so the base case: a manifest small enough to
fit in one block is replicated directly by hash with no manifest-of-manifest,
and its `n` locations are recorded in the parent snapshot.

Large manifests are a real problem. CIP-002's open question notes a 1 TiB
object at 4 MiB blocks yields ~262k blocks and a manifest in the tens of MB.
Fix it two ways:

1. **Scale block size with object size.** Target ≤4096 blocks per object:
   `block_size = max(4 MiB, next_pow2(object_len / 4096))`. A 1 TiB object gets
   256 MiB blocks and a ~600 KiB manifest. Recorded per-object in the manifest,
   so nothing is hard-coded.
2. **Chunk oversized manifests** into a two-level manifest as a normal object.

### The snapshot

A snapshot is an immutable, content-addressed map from name to object hash. In
this CIP it is a flat index; CIP-007 replaces the contents with an inode tree
without changing the mechanism.

Serialised as a **HAMT** (hash array mapped trie) of fixed-size nodes, each
node stored as its own content-addressed block. This matters more than it
sounds: a snapshot must be cheap to *update*, not just to read. With a HAMT,
changing one entry rewrites only the ~log₃₂(N) nodes on the path to the root
and shares every other node with the previous snapshot. A million-entry volume
costs ~4 node writes per change instead of rewriting a million-entry file.

Structural sharing also makes point-in-time snapshots nearly free, which is
where CIP-013's database backup story comes from.

### The root pointer

```json
{
  "volume": "vol_7f3a9c2e",
  "sequence": 41207,
  "snapshot": "blake3:...",
  "parent": "blake3:...",
  "written_at_ms": 1756512000000,
  "writer_did": "did:coinpay:...",
  "signature": "..."
}
```

`sequence` increments by exactly one per update. A reader that sees a gap knows
it is missing history; a writer that sees its expected sequence already taken
knows it lost a race (CIP-010).

Durability for the root uses three mechanisms, because it is the one thing
whose loss is unrecoverable:

1. **CoinPay anchor (authoritative).** The DID's registry entry holds the
   current root. CoinPay is already the identity, payment, and reputation layer
   under DIP-0007, so this adds no new central dependency — and DIP-0011's "no
   central backend" already names CoinPay as a source of truth.
2. **Gossipsub announcement.** Roots publish to a `c0mpute/storage/roots/v1`
   topic. Storage nodes holding the volume's shards cache the latest signed
   root they've seen. Cheap, fast, self-healing, not authoritative.
3. **Local journal.** The writing client keeps every root it has written.
   Enough to recover alone if CoinPay is unreachable at recovery time.

A root is only advanced **after** the snapshot it names is fully placed. The
ordering is: write blocks → write manifests → write snapshot nodes → advance
root. A crash anywhere before the last step leaves the previous root valid and
some unreferenced garbage, which is the correct failure mode — never a root
pointing at data that isn't there.

### Garbage collection

Immutability plus mutable roots means orphans: superseded snapshot nodes,
manifests for deleted objects, blocks nobody references.

- Roots retain their last `N` ancestors (default 32, configurable) as an undo
  history. Anything reachable from a retained root is live.
- **Mark-and-sweep, per volume, client-driven.** The client walks reachable
  hashes from retained roots and publishes a signed *keep-set digest*. Storage
  nodes hold a shard while any keep-set references it, or until a grace period
  (default 14 days) expires with no keep-set mentioning it.
- The grace period is what makes an offline client safe: a laptop that is shut
  for a week does not lose its data. It also means deletion is not instant, and
  billing must reflect held-not-referenced bytes — CIP-006's problem.
- Refcounting, per DIP-0012's open question, is *not* used: with content-
  addressed dedup across volumes, refcounts require global coordination that
  DIP-0011 rules out. Grace-period sweep is weaker but decentralised.

### Recovery

Given only the DID private key:

```
c0mpute storage recover --did did:coinpay:... --volume vol_7f3a9c2e
```

Reads the root from CoinPay, fetches the snapshot, walks manifests, verifies
shard availability, and reports what is intact, degraded, or lost — before
mounting anything. Losing every client machine costs nothing but a re-sync.

## Acceptance criteria

1. `c0mpute storage volume create` returns a volume id; `list` shows it with
   its sequence and snapshot hash.
2. 1000 sequential object writes produce 1000 root updates with strictly
   increasing sequence and no gaps.
3. Updating one entry in a 100k-entry volume writes fewer than 10 snapshot
   nodes (proves structural sharing).
4. `kill -9` the client mid-write: the root still resolves to the previous
   snapshot and every object it names reads back correctly.
5. Wipe the client's local state entirely; `recover` with only the DID key
   reconstructs the full object list and reads every object.
6. A 1 TiB object produces a manifest under 1 MB (proves block-size scaling).
7. Deleting an object and running GC frees its shards after the grace period,
   and not before.
8. A root signed by a different DID is rejected.

## Risks

- **CoinPay becomes a hard dependency for every write.** A root update per
  write means CoinPay write latency is in the filesystem's critical path.
  *Mitigation:* batch root updates — advance at most every `T` ms (default 500)
  or every `N` operations, whichever first; `fsync` forces one immediately
  (CIP-008). Gossip carries the root between anchors so readers aren't blocked.
- **CoinPay outage stalls durability.** *Mitigation:* keep writing to the local
  journal and gossip; queue the anchor. Report degraded state in `doctor`
  rather than failing writes. The data is safe; only the authoritative pointer
  lags.
- **Grace-period GC lets a departed customer's data linger, billed.**
  *Mitigation:* explicit `volume destroy` publishes a tombstone that skips the
  grace period. Billing stops at tombstone, not at sweep.
- **HAMT is real work to get right.** *Mitigation:* it is a well-specified
  structure with reference implementations; budget for property tests against a
  naive map rather than inventing anything.

## Estimate

**3–4 weeks.** ~1 week HAMT with property tests, 1 week root pointer plus
CoinPay anchoring and gossip, 0.5 week manifest-as-object and block-size
scaling, 1 week GC and keep-sets, 0.5 week recovery command.

## Open questions

- Retained-root depth of 32: enough for a useful undo window, or should it be
  time-based (e.g. 7 days of roots)?
- Should the keep-set digest be a Bloom filter to keep it small? False
  positives retain garbage, which is safe; false negatives delete live data,
  which is not — so the filter must be sized conservatively or made exact.
- Is one root per volume the right granularity, or should large volumes shard
  the root by subtree to reduce write contention? CIP-010 may force this.
