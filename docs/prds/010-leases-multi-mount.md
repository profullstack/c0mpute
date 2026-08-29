---
cip: 010
title: "Single-writer leases and multi-mount coherence"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-08-29
updated: 2026-08-29
implements: DIP-0012, DIP-0011 (no central backend)
depends-on: 009
blocks: 013
implementation:
estimate: "3–4 weeks"
---

## Summary

Define what happens when the same volume is mounted in more than one place —
which is the first thing every user will try. v1 enforces **one writer, many
readers** with an explicit, expiring, DID-signed lease, and makes the failure
modes visible instead of silently corrupting data.

## Motivation

CIPs 004–009 all assume a single writer. That assumption is load-bearing:
without it, two mounts advancing the same root pointer produce lost updates,
and two mounts caching the same inodes produce stale reads.

The user-visible reality is that people mount things twice — on a laptop and a
worker, on two workers in a job, or by accident after a crash left a lease
behind. The choice is between detecting that and handling it, or not detecting
it and losing data. Everything below follows from picking the first.

Being explicit about the limit is also the honest thing to ship. "One writer at
a time, enforced" is a real product. "Multi-writer" that silently loses writes
is not.

## Goals

- At most one writer per volume at any time, enforced not merely documented.
- Readers see a consistent, bounded-staleness view while a writer is active.
- A crashed writer's lease expires and recovers without human intervention.
- Conflicts are detected and reported, never silently resolved by overwriting.
- No central lock service (DIP-0011).

## Non-goals

- True concurrent multi-writer with merge. Deferred; see Future work.
- Byzantine writers. A writer with the volume's key can corrupt its own volume;
  this is an authorisation question, not a concurrency one.

## Design

### The write lease

```json
{
  "volume": "vol_7f3a9c2e",
  "holder_did": "did:coinpay:...",
  "holder_mount": "hostname:/mnt/data",
  "acquired_at_ms": 1756512000000,
  "expires_at_ms": 1756512060000,
  "epoch": 17,
  "signature": "..."
}
```

- **60-second term, renewed every 20 seconds.** Two missed renewals expire it.
- `epoch` increments on every acquisition. A root update signed under an old
  epoch is rejected — this is the actual enforcement, and it is what makes the
  lease more than an advisory flag.
- Stored alongside the root pointer in the CoinPay registry (authoritative) and
  gossiped on `c0mpute/storage/leases/v1` (fast path).

Acquisition: read the current lease. If absent or expired, write a new one at
`epoch+1` via CoinPay's compare-and-set on the registry entry. CoinPay is
already the authority for the root pointer (CIP-004), so this introduces no new
central dependency — it reuses the one DIP-0011 already sanctions.

If held and unexpired, the mount fails with a message naming the holder:

```
$ c0mpute storage mount vol_7f3a /mnt/data
error: volume vol_7f3a is mounted read-write by worker-3:/mnt/data
       lease expires in 41s (renewing)
hint:  mount read-only with -o ro, or use --steal if that mount is dead
```

### Read-only mounts

Readers take no lease and are unlimited. A reader polls the root pointer
(gossip, falling back to CoinPay) every `root_poll_ms` (default 2000) and
advances its view atomically when the sequence increases.

Because every layer below the root is immutable and content-addressed, a reader
holding a snapshot hash has a **consistent point-in-time view** for free. There
is no torn state: it sees snapshot `N` entirely, then snapshot `N+1` entirely.
Staleness is bounded by the poll interval and is reported by `statfs`.

This is the strongest property the CIP-004 design gives us, and it means "many
readers" costs almost nothing to support correctly.

### Stealing a dead writer's lease

The common case: a laptop crashed while holding a lease.

```
c0mpute storage mount vol_7f3a /mnt/data --steal
```

Permitted only when the lease has expired. It bumps the epoch, which fences the
old writer permanently — if that machine wakes up, its next root update is
rejected on epoch and it enters the recovery path below rather than corrupting
anything.

`--steal` never applies to an unexpired lease. Waiting 60 seconds is the price
of not having a distributed consensus protocol, and it is the right trade.

### The fenced writer

A writer whose lease expired (long GC pause, network partition, laptop sleep)
may hold journalled, un-uploaded writes. On rejection it must not discard them.

1. Stop accepting new writes; the mount goes read-only immediately.
2. Report loudly via `status` and a `dmesg`-visible FUSE error.
3. Preserve the journal, and export the divergent state:

```
$ c0mpute storage status vol_7f3a
FENCED: lease lost at epoch 17; volume now at epoch 18 (worker-3)
        412 MB / 38 files written locally after divergence
        recover with: c0mpute storage export-divergent vol_7f3a ./recovered
```

4. `export-divergent` writes the un-uploaded files to a local directory as
   ordinary files, so nothing is lost even though it cannot be merged.

Deliberately **no automatic merge.** Two divergent filesystem trees cannot be
merged safely without application knowledge — that is exactly the mistake that
makes distributed filesystems infamous. Surface it, preserve it, let a human
decide.

### Coherence for readers during writes

A reader advancing from snapshot `N` to `N+1` invalidates only cache entries
whose hash changed, which the HAMT makes cheap to compute: walk the two roots
and diff, pruning wherever the node hashes match. A typical advance touches a
handful of paths.

Open file handles on a reader keep their inode-at-open, matching NFS
close-to-open semantics. An application wanting the new version reopens. This
is well-trodden behaviour that users already understand from NFS.

### Advisory locking

`flock(2)` and `fcntl` locks are honoured **within a single mount** by the
local FUSE layer. Across mounts they are not, because there is only ever one
writer — cross-mount write locks would be meaningless.

`statfs` reports `single_writer` so applications can detect the model. CIP-013
depends on this: a database using `flock` for its own safety must be told
whether that lock spans mounts. It does not, and saying so plainly is what
keeps someone from running two Postgres instances against one volume.

## Acceptance criteria

1. Two read-write mounts of one volume: the second fails, naming the first.
2. `kill -9` the writer; after 60s another host mounts with `--steal`; all data
   written before the crash is intact.
3. `--steal` against a live, renewing lease is refused.
4. A fenced writer goes read-only within one renewal interval, loses no
   journalled data, and `export-divergent` recovers every post-divergence file.
5. Ten concurrent read-only mounts during a heavy write load: none observes a
   torn snapshot; staleness stays within `root_poll_ms` + anchor latency.
6. Reader cache invalidation on a root advance touches O(changed paths), not
   O(volume) — asserted via a counter.
7. Network partition of the writer for 5 minutes: it fences itself; the volume
   stays available read-only; no split-brain root updates land.
8. `statfs` reports the single-writer model and current staleness.
9. `flock` within one mount excludes correctly; the docs state it does not
   across mounts.

## Risks

- **Users expect multi-writer and will be disappointed.** Two people editing
  files on one volume is an obvious ask. *Mitigation:* be explicit everywhere —
  CLI, docs, `statfs`. A clear limit beats a vague promise. Note that S3-style
  access (CIP-012) has no such limit for object writes, which covers a good
  share of the demand.
- **CoinPay latency in the lease renewal path.** A slow anchor could fence a
  healthy writer. *Mitigation:* gossip is the fast path; renewal starts at
  one-third of the term, giving three attempts before expiry.
- **Clock skew.** Leases are wall-clock. A writer with a fast clock may believe
  it is fenced early; a slow one may believe it still holds a lease it lost.
  *Mitigation:* the epoch check is the real enforcement and is clock-free —
  time only decides when a *new* holder may take over, and the 60s term absorbs
  ordinary skew. Nodes with skew beyond 5s are flagged by `doctor`.
- **`--steal` used carelessly on a live-but-partitioned writer.** *Mitigation:*
  refuse while unexpired; the epoch fence makes the outcome safe even when it
  is used, at the cost of the divergent-export dance.

## Future work: real multi-writer

If demand justifies it, the path is sharded roots: split the volume's namespace
into subtrees, each with its own root pointer and lease, so writers working in
different directories never contend. Cross-subtree `rename` then needs a
two-phase protocol. That is a substantial project — CRDT-style directory merge
plus a distributed rename protocol — and should be its own DIP, not an
extension of this CIP.

## Open questions

- Is 60 seconds the right term? Shorter recovers faster from crashes; longer
  tolerates worse networks. 60s with 20s renewal is a starting point, not a
  measured one.
- Should read-only mounts optionally pin a snapshot (never auto-advance) for
  reproducible job inputs? Cheap to add and useful for training runs.
- Should `--steal` require a second confirmation when the holder was seen alive
  within the last term?
