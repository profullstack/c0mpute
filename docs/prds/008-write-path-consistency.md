---
cip: 008
title: "Write path: chunking, journal, and crash consistency"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-08-29
updated: 2026-08-29
implements: DIP-0012
depends-on: 007
blocks: 009, 013
implementation:
estimate: "4–5 weeks"
---

## Summary

Make writes fast enough to be usable and honest enough to be trusted. A local
write-back journal on the client's own disk absorbs writes at local-disk speed;
a background uploader turns journalled data into placed shards; and `fsync(2)`
means something specific and documented rather than something hopeful.

This is the CIP where the read/write product is won or lost. Everything else is
plumbing by comparison.

## Motivation

Applications assume filesystem writes are fast and that `fsync` means durable.
Neither is naturally true over a network of consumer nodes with 200–500 ms
latencies:

- A naive synchronous write costs a full RS encode plus `n` peer round trips —
  hundreds of milliseconds for a 4 KiB write. `tar -x` would take days.
- If `fsync` returns before data is placed, a crash loses acknowledged writes
  and every durability claim in the product is false.

The reconciliation is a local journal: acknowledge into durable local storage,
upload asynchronously, and define `fsync` in terms of what has actually
happened rather than what we would like to have happened.

## Goals

- Buffered writes complete at local-disk latency.
- `fsync` provides a precise, documented durability guarantee.
- A client crash never loses data that `fsync` acknowledged.
- A client crash never leaves the volume inconsistent.
- Bounded, observable divergence between local and network state.

## Non-goals

- Multi-writer coordination (CIP-010).
- Making `fsync` fast. It is a network round trip and will cost one.

## Design

### The journal

Every mount owns a local journal on ordinary local disk (default
`~/data/c0mpute/journal/<volume>`), which should be the fastest device
available — the mount's write performance is the journal's write performance.

```
journal/
  wal/000042.log        append-only intent records
  staging/<hash>        chunk payloads awaiting upload
  state.db              upload queue, dirty inodes, sequence watermarks
```

Records are append-only and CRC'd:

```rust
enum JournalRecord {
    ChunkStaged { hash: Hash, len: u32 },
    ChunkPlaced { hash: Hash, hosts: Vec<PeerId> },
    InodeUpdate { ino: u64, inode: Inode },
    DirUpdate   { parent: u64, name: String, entry: Option<(u64, FileKind)> },
    RootAdvanced { sequence: u64, snapshot: Hash },
    Barrier { id: u64 },
}
```

### Write path

```
write(2)
  └─► write into page cache, mark inode dirty          ~µs, returns
        └─► chunker seals a chunk (FastCDC boundary or flush)
              └─► append ChunkStaged + payload to journal   ~local disk
                    └─► uploader: RS-encode, place n shards (CIP-003)
                          └─► append ChunkPlaced
                                └─► root advance batches dirty inodes (CIP-004)
                                      └─► append RootAdvanced, free staged payload
```

Writes return at the second step. Everything after is asynchronous, rate-limited
and observable.

### What fsync means

Three modes, chosen per-mount, because there is no single right answer and
pretending otherwise is how people lose data:

| Mode | `fsync` returns when | Survives | Latency |
|---|---|---|---|
| `local` | journal `fdatasync`'d to local disk | client crash / power loss | ~1 ms |
| `network` (**default**) | shards placed at write-ack quorum (CIP-003: `k + parity/2`) **and** root advanced | total client loss | ~200–800 ms |
| `paranoid` | all `n` shards placed and CoinPay anchor confirmed | client loss + slow peers | ~1–3 s |

**`network` is the default** because it is the only mode matching what an
application means by "durable" in a distributed system: the data survives the
machine that wrote it. `local` is offered for workloads that fsync constantly
and treat the network as a replica — it is honest, but it does not survive
losing the client, and the CLI says so at mount time.

The mode is reported in `statfs` and by `c0mpute storage status`, so a database
can be configured against a known guarantee rather than a guess. CIP-013 depends
entirely on this table being accurate.

### Crash consistency

The invariant: **the volume root never references data that is not placed.**
CIP-004 already orders block → manifest → snapshot → root. The journal
preserves that ordering across a crash.

On mount after an unclean shutdown:

1. Scan the WAL forward from the last `RootAdvanced`.
2. Re-stage any `ChunkStaged` without a matching `ChunkPlaced`; re-queue upload.
3. Rebuild dirty inode state from `InodeUpdate`/`DirUpdate` after the watermark.
4. Compare local root sequence with the volume's anchored root:
   - local ahead → replay to catch the network up (normal case: crash after
     journalling, before anchoring).
   - local behind → another writer advanced it; CIP-010 conflict path.
   - equal → clean.
5. Sweep orphan inodes (CIP-007's unlink-while-open list).

Recovery is bounded by journal size, not volume size. A 10 TB volume with a
2 GB journal recovers in seconds.

### Bounding divergence

An unbounded journal is a lie: it looks like a working filesystem while
accumulating data that exists on exactly one disk — precisely the failure the
product claims to prevent.

- `journal_max_bytes` (default 8 GB) and `journal_max_lag_secs` (default 300).
- Approaching either, writes are **throttled** to the measured upload rate.
- At the limit, writes block (`EAGAIN` for non-blocking) rather than silently
  buffering more.
- `c0mpute storage status` always shows journal depth, upload rate, and
  estimated drain time. Users must be able to see how far behind they are.

Backpressure over silent buffering is a deliberate choice: a slow filesystem is
an annoyance, a filesystem that loses a day's work on a laptop failure is a
product failure.

### Read-your-writes

Reads consult the journal first, then the network. A just-written chunk still
staging is served from local staging, so an application never sees a write it
made disappear. Consistency within a mount is total, regardless of upload lag.

### Batching root advances

One root advance per write would mean one CoinPay anchor per write. Batch:
advance at most every `root_batch_ms` (default 500) or every 1000 dirty inodes,
whichever first. `fsync` in `network`/`paranoid` mode forces an immediate
advance. Between advances the journal is authoritative and safe.

## Acceptance criteria

1. `dd if=/dev/zero of=<mount>/f bs=4k count=100000` sustains within 20% of the
   same write to the journal's local device.
2. `fsync` in `network` mode does not return until shards are at quorum: kill
   the client immediately after `fsync` returns, mount elsewhere, and the data
   is present. 1000 iterations, zero losses.
3. `fsync` in `local` mode survives `kill -9` but is documented as not
   surviving client loss; a test asserts this distinction explicitly.
4. Power-loss simulation (`dm-flakey` or equivalent) 1000 times: the volume is
   always consistent; no root references a missing block.
5. `tar -xf linux-6.x.tar.xz` into the mount completes within 3x of the same
   extraction on local disk.
6. Filling the journal throttles rather than erroring, and blocks rather than
   buffering unbounded; `status` shows lag throughout.
7. Recovery from a 2 GB dirty journal completes in under 30 seconds.
8. Read-your-writes holds under a concurrent write/read loop with uploads
   artificially stalled.
9. `fio` random 4 KiB writes with `fsync=1` in `network` mode reports latency
   consistent with the documented table (not silently faster, which would mean
   fsync is lying).

## Risks

- **Journal device failure loses un-uploaded data.** The window is real and
  proportional to lag. *Mitigation:* bound the lag; document it; offer
  `journal_mirror` to a second local device for critical mounts.
- **`fsync` latency makes some applications unusable.** SQLite in rollback mode
  fsyncs per transaction; at 500 ms that is 2 tps. *Mitigation:* this is
  physics, not a bug — CIP-013 documents which database configurations are
  viable rather than pretending all are.
- **FastCDC plus journaling is CPU-heavy on a worker also running inference.**
  *Mitigation:* cgroup the mount's CPU; make chunking parameters tunable.
- **Batched root advances widen the crash window in `local` mode.** Up to
  `root_batch_ms` of metadata lives only in the journal. *Mitigation:* correct
  by design (journal is durable and replayed), but it must be understood when
  reasoning about `local` mode.
- **Divergence throttling will be perceived as the product being slow.**
  *Mitigation:* surface *why* in `status` and in the throttle message. A slow
  filesystem that explains itself is tolerable.

## Estimate

**4–5 weeks.** ~1 week journal format and replay, 1 week the async uploader and
backpressure, 0.5 week the three fsync modes, 0.5 week read-your-writes and
staging reads, 1 week crash-consistency test harness (`dm-flakey`), 1 week
performance work against the acceptance targets.

## Open questions

- Should `network` fsync wait for the root anchor, or only for shard quorum
  plus a journalled root? Waiting on CoinPay adds latency for a guarantee the
  gossip layer largely provides.
- Is 8 GB the right default journal cap? It should probably be a fraction of
  free space rather than an absolute.
- Should `local` mode even be offered, given how easy it is to misread? It is
  genuinely right for scratch space on a worker, but it is also the mode that
  will be blamed when data is lost.
