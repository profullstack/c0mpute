---
dip: 0017
title: "Streaming reads: workers consume objects while shards are still arriving"
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

Extend the DIP-0012 v3 storage primitive with a **sequential-systematic
RS layout** that lets workers start consuming an object's bytes as
shards arrive, instead of waiting for full reconstruction. For
sequential workloads (transcode reading frames in order, AI inference
loading model weights, training pipelines streaming a dataset), this
eliminates the "wait for full reassembly" stall and cuts job
time-to-first-byte from `T(slowest_shard_fetch)` to
`T(first_shard_arrives + reconstruction_throughput)`.

Opt-in via a `layout: "sequential"` flag on the object manifest. The
existing default RS layout stays available for random-access
workloads.

## Motivation

The current RS 10/14 layout interleaves bytes across shards at symbol
granularity (standard RS striping). Reconstructing *any* range of the
original plaintext requires fetching 10 shards in full. A worker that
will read the object linearly — most ML and media workloads — sits
idle until the slowest of those 10 fetches completes. On a network
with realistic shard-host churn that "slowest of 10" tail latency
dominates job startup.

Three workloads where the stall is the bottleneck right now:

1. **Transcode.** ffmpeg starts consuming the input from byte 0 and
   reads sequentially. Today the worker downloads the full input
   first, then invokes ffmpeg. With streaming layout, ffmpeg starts
   the moment the first prefix is reconstructable; encode time
   overlaps with the long-tail shard fetches.
2. **LLM inference on a c0mpute-stored model.** Loading a 70B-param
   weights file into VRAM is read-once and sequential. Today the
   worker waits for the full ~140 GB reassembly before generation can
   begin. Streaming makes load-into-VRAM overlap with reassembly.
3. **Training data pipelines.** A worker iterating over a multi-TB
   dataset reads it once, sequentially, then discards. The whole-file
   stall doesn't help anything — it just delays the first batch.

BitTorrent solved a version of this in 2001 with sequential piece
prioritization in clients like µTorrent (the "streaming" mode). We
need the same shape, expressed through RS reconstruction instead of
naked piece-fetch ordering.

## Detailed design

### The RS layout problem

A standard RS 10/14 code over a buffer produces 14 shards where each
shard contains a symbol from every block of the input. To reconstruct
*any* contiguous range, you need 10 shards in full. There is no way
to read "byte 0..N" from "shard 0 alone" — shard 0 contains every
10th symbol of the original.

This is fine for durability but hostile to streaming reads.

### Sequential-systematic layout

A **systematic** RS code keeps the original data bytes unchanged in
the first K data shards, and stores only parity in the remaining
N-K shards. If we further commit to laying out the data shards
**sequentially in byte order** — shard 0 = bytes `[0, L)`, shard 1 =
bytes `[L, 2L)`, ..., shard 9 = bytes `[9L, 10L)` for an object of
length `10L` — then a worker can consume bytes as soon as the
relevant data shard arrives.

```
Default layout (interleaved):
  shard 0:  [s0 s10 s20 s30 ...]    ← can't read first byte without all 10
  shard 1:  [s1 s11 s21 s31 ...]
  ...
  shard 9:  [s9 s19 s29 s39 ...]
  shard 10: parity_0
  ...

Sequential layout (this DIP):
  shard 0:  [bytes 0..L)              ← reading shard 0 yields plaintext
  shard 1:  [bytes L..2L)
  ...
  shard 9:  [bytes 9L..10L)
  shard 10: parity_0  (over same blocks)
  ...
```

Durability is unchanged: any 10 of the 14 shards still reconstruct
the object. The cost is purely on the read path — if a data shard
is unavailable, the worker must wait for a parity shard and
reconstruct that range via RS arithmetic instead of reading it
directly.

### Manifest extension

Object manifests carry a new `layout` field:

```json
{
  "object_hash": "blake3:...",
  "layout":      "sequential",
  "block_size":  1048576,
  "shards": [ ... ]
}
```

- `layout: "interleaved"` (default) — existing behavior, optimal for
  random-access reads, no streaming.
- `layout: "sequential"` — this DIP. Sequential data-shard byte
  layout, opt-in.
- `block_size` — granularity. RS striping happens within each block;
  consecutive blocks are laid out across the data shards in
  sequence. Default 1 MiB; tunable per-object.

The manifest is content-addressed by the same blake3 hash as before;
`layout` is part of the digest preimage, so a sequential-layout
object has a different hash than the same plaintext stored
interleaved.

### Worker read path

```
┌────────────────── streaming read ──────────────────┐
│                                                     │
│  Worker opens N=10 parallel shard fetches.          │
│  Each fetch streams its shard as a byte source.     │
│                                                     │
│  A "stitcher" reads from data shards in offset      │
│  order, emitting a unified byte stream to the       │
│  workload (ffmpeg stdin, torch DataLoader, etc.).   │
│                                                     │
│  When a data shard stalls (no bytes for T_stall):   │
│    – Switch to a parity shard.                      │
│    – Reconstruct the missing data-shard range via   │
│      RS arithmetic from already-arrived bytes +     │
│      parity bytes for that block.                   │
│    – Emit the reconstructed range to the workload.  │
│                                                     │
│  Cost: stall-free reads cost O(1) per byte.         │
│        parity-fallback reads cost O(K) per byte for │
│        affected ranges. Acceptable.                 │
└─────────────────────────────────────────────────────┘
```

### Stitcher API

```rust
// c0mpute-store::stream
pub fn streaming_read(
    object_hash: &Hash,
    manifest: &Manifest,
    shard_sources: ShardSourceSet,
) -> impl AsyncRead { /* stitcher */ }
```

The returned `AsyncRead` is hooked into the workload — for transcode,
it's piped to `ffmpeg`'s stdin via the existing module-dispatch
plumbing (DIP-0006); for inference, into the model loader.

### Job manifest signal

A job manifest opts in by declaring its read pattern:

```json
{
  "input": {
    "uri":           "c0mpute://blake3:...",
    "layout":        "sequential",
    "read_pattern":  "linear"
  }
}
```

`read_pattern: "linear"` is the signal that the streaming path is
worthwhile. `read_pattern: "random"` (default) keeps the current
"wait for full reconstruction" path even on a sequential-layout
object. Workers that don't implement streaming reads ignore the
hint and fall back to default reconstruction.

### Failure modes

- **One data shard host stalls.** Stitcher switches to a parity
  shard and reconstructs the affected range. Read continues at
  reduced throughput for that block range, returns to direct reads
  once a healthier data shard is available (or the next block).
- **Multiple data shards stall simultaneously.** Up to 4 can stall;
  more than 4 means the object is unrecoverable from the current
  shard set. Stitcher surfaces this as a read error to the
  workload, which can either fail the job or retry with fresh
  shard hosts.
- **Worker can't keep up with arrival.** Stitcher's buffer fills,
  back-pressures the shard sources via standard async-stream
  flow control. No data loss.

### Performance expectations (be honest)

Time-to-first-byte improves dramatically for sequential workloads
on large objects:

| Object | Current (full reassemble) | Streaming (this DIP) | Win |
|---|---|---|---|
| 5 GB video, transcode | wait ~20–40s for slowest of 10 fetches, then ffmpeg starts | ffmpeg starts at first-shard ETA + tiny stitcher overhead | 10–30s shaved off total job |
| 140 GB model, inference | wait ~3–8 min for full reassembly, then load to VRAM | load-to-VRAM overlaps with shard arrival | 2–6 min shaved |
| 2 TB dataset, training | wait ~30+ min before first batch | first batch on the first ~1 GB | minutes-to-hours shaved |

For random-access workloads (a SQLite db, a HDF5 chunked array
where the workload reads non-sequentially) — **streaming does not
help**, and the read path falls back to current behavior. Honest
framing in marketing: streaming is a sequential-workload win, not a
universal one.

## Alternatives considered

**Pre-fetch and wait (status quo).** What we do today. Correct for
random-access workloads, leaves performance on the table for
sequential ones.

**Per-shard speculative reads on the interleaved layout.** Try to
peek at the first symbol of each shard and reassemble byte 0
optimistically. Doesn't actually work — RS at symbol granularity
requires all K symbols of *every* block before any plaintext byte
emerges.

**Split objects into many small content-addressed pieces.** IPFS /
BT-style chunking. Loses single-object content-addressing benefits,
multiplies manifest overhead, complicates dedup. Not worth it when
sequential-systematic RS gets the same time-to-first-byte without
fragmenting the object identity.

**Fountain codes (LT / Raptor).** Random-access progressive
reconstruction. Theoretically attractive — any K-of-N decoder works
out-of-order. Real implementations have nontrivial constants, less
mature libraries than RS, and the win over sequential-systematic RS
is marginal for sequential workloads (the dominant case). Revisit
if random-access progressive reads become a real demand.

**Just pin hot objects in the worker's cache.** Helps on the second
read, doesn't help on the first read or one-shot workloads.
Orthogonal — already supported via the existing ephemeral
chunk-store, just not framed as "streaming."

## Migration & rollout

Phase 1 — **Library work, no behavior change.**
- Add `encode_sequential` / `decode_sequential` paths to
  `c0mpute-store::erasure`, alongside the existing interleaved
  encoder.
- Manifest schema gains `layout` and `block_size` fields with
  default values that preserve current behavior.
- Encode-side tests, decode-side tests, failure-mode tests.

Phase 2 — **Stitcher + worker integration via transcode.**
- `c0mpute-store::stream::streaming_read()` exposing the unified
  `AsyncRead`.
- Add a streaming variant to `c0mpute-transcode`: a
  `transcode_stream(ffmpeg_bin, input: impl AsyncRead, ...)` that
  pipes into ffmpeg's stdin (`-i -`) instead of taking a file path.
  The existing path-based `transcode()` stays for non-streaming
  callers. (Note: ffmpeg-the-binary already supports stdin; the
  current `c0mpute-transcode` wrapper does not — it assumes a
  materialized file. This phase includes adding the streaming
  variant.)
- Reshape `spawn_transcode_runner` in `c0mpute-core/runner.rs` so
  workloads with `read_pattern: "linear"` use the streaming path
  instead of the download-then-invoke path.
- Benchmark against the current file-path flow on realistic
  objects (5 GB video, 10-worker pool, simulated tail-latency).

Phase 3 — **Inference module integration.**
- Plumb the stitcher into the infernet model-loader path.
  Coordinate with infernet-protocol on the model file loader's
  read pattern. Benchmark on a realistic large model.

Phase 4 — **Customer-facing.**
- `c0mpute storage put --layout sequential` CLI flag.
- Job manifest `read_pattern: "linear"` documented.
- Pricing page note: no price difference (it's a layout choice,
  not a tier).

Each phase is independently shippable and reversible — the default
layout stays interleaved, so any phase can land without changing
existing object behavior.

## Open questions

- **Block size default.** 1 MiB is a reasonable starting point but
  the optimum depends on shard-fetch round-trip and workload
  consumption rate. Should land with a benchmark sweep before
  Phase 4.
- **Cross-shard alignment.** Do the 10 data-shard hosts need to send
  block-aligned chunks for the stitcher to make progress, or can
  they send arbitrary byte ranges? Implementation says arbitrary
  ranges work if the stitcher reorders, but block-aligned is much
  simpler. Probably mandate block-aligned for v1.
- **Parity-reconstruction throughput.** Standard RS reconstruction is
  CPU-bound; at multi-GB/s shard arrival rates it can become the
  bottleneck. Mitigation: SIMD-accelerated RS libraries (already in
  use elsewhere in the ecosystem). Confirm performance headroom
  before Phase 2 closes.
- **Interaction with E2E `private` tier (DIP-0012 v3).** Sequential
  layout is orthogonal to encryption — the customer-side encryption
  happens on the plaintext stream before sharding, so sequential
  byte ranges remain sequential under encryption. Confirm no
  subtle interaction with the chosen cipher mode.
- **Random-access streaming for non-linear workloads.** Out of
  scope for this DIP, but worth flagging as a future research
  direction (fountain codes, partial-block parity).

## Out of scope

- Replacing the default interleaved layout — both layouts coexist,
  customers choose per-object.
- Streaming *writes* (progressive upload). Possible later, not
  needed for the current motivating workloads.
- Multi-object streaming compositions (e.g., "stream this list of
  objects in order"). Workload-level concern; outside the storage
  primitive.
- Re-encoding existing interleaved objects to sequential layout.
  Customers re-upload if they want the new layout. Bulk migration
  tooling is not a v1 concern.
- Fountain-code-based progressive random-access reads. Interesting
  future work; deferred.
