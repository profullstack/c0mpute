---
cip: 002
title: "Storage HTTP API on the gateway"
status: In progress
authors:
  - anthony@profullstack.com
created: 2026-08-29
updated: 2026-08-29
implements: DIP-0012 (0012-storage-plugin.md) Phase 2
depends-on: 001
blocks: 003, 004, 012
implementation: PR #21 (c0mpute-store block layer + manifest v2, c0mpute-gateway storage API, `c0mpute storage` CLI)
estimate: "1.5–2 weeks"
---

## Summary

Expose the already-working `c0mpute-store::Storage` engine over HTTP so
something other than a unit test can put and get an object. Single node only —
all 14 shards land on the local disk and `host_hint` stays `None`. This is the
smallest change that turns shipped library code into a usable service.

## Motivation

`c0mpute-store` has erasure coding, manifests, integrity verification, and nine
passing tests. The gateway (`c0mpute-gateway/src/lib.rs`) currently exposes
exactly two routes:

```rust
Router::new()
    .route("/healthz", get(healthz))
    .route("/chunks/{hash}", get(chunk_handler))
```

There is no way to reach the storage engine from outside the process. Every
later phase — placement, repair, the filesystem, S3 — needs this surface, and
none of them can be built or tested without it.

## Goals

- PUT/GET/DELETE/HEAD an object by content hash over HTTP.
- PUT/GET a single shard, which is the primitive CIP-003 uses for placement and
  CIP-005 uses for repair.
- Streaming request and response bodies — no `Vec<u8>` of a whole object.
- Signed-request auth on writes, per DIP-0007.
- Per-tier `(k, n)` selection from CIP-001.

## Non-goals

- Cross-node placement (CIP-003). `host_hint` is `None` throughout.
- Repair (CIP-005), challenges or billing (CIP-006).
- Mutable paths, directories, or names (CIP-007). Objects are content-addressed
  and immutable here.
- S3 wire compatibility (CIP-012).

## Design

### Routes

Added to `c0mpute-gateway`, behind the `storage` role being enabled:

```
PUT    /storage/v1/objects/{object_hash}   store an object
GET    /storage/v1/objects/{object_hash}   reconstruct and stream it back
HEAD   /storage/v1/objects/{object_hash}   existence + length, no body
DELETE /storage/v1/objects/{object_hash}   drop manifest + unreferenced shards

PUT    /storage/v1/shards/{shard_hash}     accept one shard (peer placement)
GET    /storage/v1/shards/{shard_hash}     serve one shard
HEAD   /storage/v1/shards/{shard_hash}     do you hold it?

GET    /storage/v1/manifests/{object_hash} the manifest as JSON
```

`{object_hash}` is `blake3:<hex>`; a bare hex string is also accepted.

### Object PUT

```http
PUT /storage/v1/objects/blake3:9f86d0... HTTP/1.1
X-Coinpay-Auth: base64url(envelope)
X-C0mpute-Tier: standard          ; hot | standard | critical, default standard
Content-Type: application/octet-stream
Content-Length: 1048576
```

The client commits to the hash in the URL. The server streams the body to a
temp file, hashes as it goes, and **rejects with 422 if the computed hash does
not match the URL**. This is what makes the store trustworthy without trusting
the uploader — it is the same property `ChunkStore::get` already enforces on
read.

Response `201 Created` returns the manifest as JSON (the `ObjectManifest`
struct already derives `Serialize`).

Idempotent: PUT of an object that already exists returns `200 OK` with the
existing manifest and writes nothing.

### Streaming is mandatory, not an optimisation

`Storage::put(&self, data: &[u8])` takes a full slice, and `get` returns
`Vec<u8>`. For a filesystem backend that is untenable — a 4 GiB file would mean
a 4 GiB allocation on both ends, and the c0mpute worker rigs this is aimed at
also run inference.

This CIP adds streaming variants alongside the existing ones:

```rust
impl Storage {
    /// Consume a byte stream, hashing and RS-encoding block by block.
    pub async fn put_stream<S>(
        &self, stream: S, expected: Option<Hash>, tier: Tier, size_hint: Option<u64>,
    ) -> Result<ObjectManifest>
    where S: Stream<Item = Result<Bytes>>;

    /// Yield the object's blocks in order, reconstructing lazily.
    pub fn read_stream(&self, manifest: ObjectManifest)
        -> Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

    /// Byte-range read. Needed by CIP-007 for random-access files.
    pub async fn get_range(
        &self, object_hash: &Hash, offset: u64, len: u64,
    ) -> Result<Vec<u8>>;
}
```

A `Stream<Item = Result<Bytes>>` rather than `AsyncRead`: axum bodies are
already byte streams in both directions (`Body::into_data_stream`,
`Body::from_stream`), so this avoids a bridging dependency on both sides.
`size_hint` carries the HTTP `Content-Length` through to [`block_size_for`].

The non-streaming `put` is a thin wrapper over `put_stream`, so there is one
write path rather than two that drift.

Both are built on a **block layer**: an object is split into fixed-size blocks
(default 4 MiB, recorded in the manifest) and each block is independently
RS-encoded into `n` shards. Consequences, all of which later CIPs depend on:

- Memory is bounded by block size, not object size.
- `get_range` fetches only the blocks a range touches, so random access does
  not read the whole file. CIP-007 cannot exist without this.
- A single damaged block is repairable without touching the rest of the object.

This means `ObjectManifest` grows a block dimension. Bump it to a versioned
format now, while nothing depends on it:

```rust
pub struct ObjectManifest {
    pub version: u8,             // NEW: 2
    pub object_hash: Hash,
    pub original_len: u64,
    pub block_size: u32,         // NEW: bytes per block, default 4 MiB
    pub k: u8,
    pub parity: u8,
    pub tier: Tier,              // NEW
    pub blocks: Vec<BlockEntry>, // NEW: replaces flat `shards`
}

pub struct BlockEntry {
    pub index: u32,
    pub len: u32,                // pre-padding plaintext length
    pub shards: Vec<ShardEntry>, // the existing struct, unchanged
}
```

Version 1 manifests (flat `shards`, single implicit block) still parse — a
`#[serde(default)]` shim maps them to a one-block v2. There is no production
data to migrate, but the shim keeps the existing tests meaningful.

### Rollback must delete only what the write created

A write that fails its hash commitment has to undo itself, and the obvious
implementation — remember every shard hash written, then delete them all — is
**wrong in a way that loses data**.

Shards are content-addressed and therefore shared between objects. Uploading
the bytes of an object that *already exists*, under a wrong committed hash,
produces exactly the same shard hashes. Rolling back everything the write
touched deletes the intact object's shards: one malformed request, from anyone
who can obtain the content, destroys it.

So `ChunkStore` grows `put_new`, which reports whether a call created the chunk
or found it already present, and rollback removes only newly-created hashes.
Refcounting is still deliberately avoided (CIP-004); this is strictly narrower
and needs no coordination.

Found by driving the running server with curl, not by the unit tests — those
stored nothing beforehand, so there was nothing for the bad write to destroy.
Both the store and the HTTP suite now carry a regression test that stores an
object first. Worth remembering for CIP-005 and CIP-004, which both delete
content-addressed data and will meet the same trap.

### Auth

Writes (`PUT`, `DELETE`) require the DIP-0007 signed-request envelope in
`X-Coinpay-Auth`. Reads of `standard`/`hot` objects are unauthenticated —
knowing a blake3 hash is itself the capability. `private`, client-encrypted
objects (CIP-011) are also served unauthenticated because the bytes are
ciphertext; confidentiality comes from the key, not the ACL.

Shard endpoints (`PUT /shards/...`) require auth from a peer whose DID is a
known network member, so a stranger cannot fill our disk. Rate-limited per DID.

### Errors

| Code | When |
|---|---|
| 400 | Malformed hash, bad tier, missing length |
| 401 | Missing or invalid envelope on a write |
| 404 | No manifest, or shard not held |
| 409 | PUT in flight for the same hash |
| 413 | Object above `max_object_bytes` |
| 422 | Body hash != URL hash |
| 507 | Disk budget exhausted |

## Acceptance criteria

1. `curl -X PUT --data-binary @file` then `curl -O` round-trips a 1 GiB file
   with a matching sha256, and the node's RSS stays under 200 MB throughout.
2. PUT with a deliberately wrong hash in the URL returns 422 and leaves nothing
   on disk.
3. `GET` with `Range: bytes=1000000-1004095` returns exactly 4096 bytes and
   fetches only the blocks covering that range (assert via a shard-read counter
   in tracing).
4. Deleting 4 of 14 shards of one block still serves the whole object; deleting
   5 returns 500 with a decode-shortage error naming the block.
5. Existing `c0mpute-store` tests pass unmodified; a v1 manifest fixture still
   deserialises.
6. Writes without a valid envelope get 401; reads don't need one.
7. `c0mpute doctor` reports the storage role's disk budget and current usage.

## Risks

- **The manifest format change ripples.** Doing it now, before CIP-003/004/007
  exist, is deliberately the cheapest moment. *Mitigation:* version field plus
  the v1 shim; land this before anything else consumes manifests.
- **`Storage` gains a second, near-duplicate code path.** *Mitigation:* make the
  non-streaming `put`/`get` thin wrappers over the streaming ones rather than
  maintaining both.
- **Blocks change dedup granularity.** Content-addressed shards dedup within a
  block boundary only; a file that shifts by one byte re-encodes entirely.
  Content-defined chunking would fix it and is deferred to CIP-007, which has
  the write patterns to justify it.

## Estimate

**1.5–2 weeks.** Roughly: 3 days for the block layer and manifest v2, 3 days
for streaming put/get plus range reads, 2 days for routes and auth wiring,
2 days for tests and `doctor` integration.

## Open questions

- `max_object_bytes` default. 4 MiB blocks make large objects tractable, but
  the manifest itself grows ~14 shard entries per block — a 1 TiB object means
  ~262k blocks and a manifest in the tens of MB. CIP-004 needs to handle large
  manifests, or block size must scale with object size.
- Does `hot` (3-copy) even use the RS path with k=1, or a separate replication
  path? k=1/parity=2 is degenerate but correct RS, and reusing the code is
  tempting. Measure before deciding.
