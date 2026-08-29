---
cip: 007
title: "c0mputefs: mutable filesystem over immutable content"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-08-29
updated: 2026-08-29
implements: DIP-0012 — supersedes its "filesystem-style mutable objects" out-of-scope line
depends-on: 004
blocks: 008, 009, 011
implementation:
estimate: "4–6 weeks"
---

## Summary

Build the inode and directory layer that turns content-addressed immutable
blocks into a POSIX filesystem with paths, permissions, and mutation. This is
the conceptual centre of the read/write product and the piece that does not
exist in any form today.

## A scope change this CIP makes explicit

DIP-0012 currently lists under **Out of scope**:

> Filesystem-style mutable objects. Content-addressed, immutable.

The decision to ship a read/write mount overrides that line. This is a genuine
reversal, not an interpretation, and it should land as a short DIP amendment
alongside this CIP rather than being buried in a PRD. The reasoning is
recorded here so the amendment can cite it:

- Content-addressed immutability remains true of **everything below the root
  pointer**. Blocks, manifests, and snapshot nodes are still immutable.
- Mutability is confined to advancing one signed 32-byte pointer per volume
  (CIP-004). The storage layer does not become mutable; a naming layer is added
  on top of it.
- So the original property that made the design tractable is preserved. What
  changes is that we now also ship the mutable index that customers were
  otherwise going to have to build themselves.

Worth stating plainly: a POSIX mount is a materially harder product than an
object store, and most of the difficulty lands in CIP-008 and CIP-010 rather
than here. This CIP is the tractable part.

## Goals

- Full POSIX metadata: inodes, modes, uid/gid, timestamps, link counts.
- Directories with atomic `rename(2)`, including the overwrite case.
- Files with random-access read and write, `truncate`, sparse regions.
- Hard links and symlinks.
- Metadata operations that complete in microseconds, not network round trips.
- Efficient small files — the pathological case for erasure-coded storage.

## Non-goals

- The write path's durability, journaling, and crash consistency (CIP-008).
- FUSE bindings and the CLI (CIP-009).
- Concurrent writers across mounts (CIP-010).
- Encryption (CIP-011).
- `O_DIRECT`, `mmap` shared-writable, mandatory locking. See Out of scope.

## Design

### Layering

```
     VFS / FUSE  (CIP-009)
          │
     c0mputefs   inodes, dentries, permissions, path resolution   ← this CIP
          │
     Volume      root pointer + HAMT snapshot  (CIP-004)
          │
     Objects     manifests, blocks, RS shards  (CIP-002/003)
```

New crate: `node/crates/c0mpute-fs`. It depends on `c0mpute-store` and the
volume layer, and knows nothing about FUSE — which keeps it testable without a
mount and reusable by the S3 gateway (CIP-012) and by CIP-013's backup tooling.

### Inodes

```rust
pub struct Inode {
    pub ino: u64,
    pub kind: FileKind,          // File | Dir | Symlink
    pub mode: u32,               // POSIX permission bits
    pub uid: u32, pub gid: u32,
    pub nlink: u32,
    pub size: u64,
    pub atime: u64, pub mtime: u64, pub ctime: u64,
    pub content: Content,
    pub xattrs: BTreeMap<String, Vec<u8>>,
}

pub enum Content {
    /// Small files live inline in the inode. No manifest, no shards.
    Inline(Vec<u8>),
    /// Larger files reference an extent tree of content-addressed blocks.
    Extents(ExtentTree),
    /// Directory entries: name -> ino, as a HAMT for large directories.
    Dir(HamtRef),
    /// Symlink target.
    Link(String),
}
```

The inode table is itself a HAMT keyed by `ino`, stored in the volume snapshot
(CIP-004). Updating one inode rewrites ~4 HAMT nodes, not the table.

### Inline small files — the small-file problem, solved by avoiding it

A 2 KiB file under RS 10/14 becomes 14 shards of ~200 bytes on 14 different
machines, plus a manifest larger than the file. It is absurd, and it is the
single most common file size on a real filesystem.

Files at or below `inline_max` (default **64 KiB**) are stored **directly in
the inode**, which lives in the snapshot HAMT, which is stored at the `hot`
tier (3-copy). Consequences:

- No manifest, no erasure coding, no 14-way fan-out for small files.
- Reads come from the already-cached snapshot — often zero network round trips.
- Replication's 1x repair amplification applies, which is what CIP-001 said
  replication was for.

Above `inline_max`, content moves to an extent tree. Crossing the threshold in
either direction is a normal rewrite.

### Extent trees

```rust
pub struct ExtentTree { pub extents: Vec<Extent> }

pub struct Extent {
    pub file_offset: u64,
    pub len: u64,
    pub content: ExtentContent,
}

pub enum ExtentContent {
    Block { hash: Hash, block_offset: u32 },  // content-addressed block
    Zero,                                     // sparse hole, stores nothing
}
```

An extent maps a byte range of the file onto a range of an immutable block.
Overwriting the middle of a file does not rewrite the file: it writes one new
block and splices three extents (prefix, new, suffix). `truncate` drops or
splits extents. Sparse files cost nothing.

This is why CIP-002's `get_range` is a hard dependency — extents are useless if
reading 4 KiB requires reconstructing a whole object.

### Content-defined chunking

CIP-002 deferred this and the write path now forces the question. With fixed
block boundaries, inserting one byte at the start of a file re-encodes every
subsequent block. With **content-defined chunking** (rolling hash, FastCDC —
target 4 MiB, min 1 MiB, max 16 MiB) boundaries follow content, so an insert
changes one chunk and the rest dedup against what is already stored.

For an append-only workload — logs, datasets, database WAL — this is the
difference between rewriting the file and appending a chunk. Adopt FastCDC for
extent content. Fixed blocks remain available for objects written through the
plain object API (CIP-002), which has no insert semantics.

### Directories and rename

Directory entries are a HAMT from name to `(ino, kind)`, so a directory with a
million entries costs `log₃₂(N)` node writes per change and `readdir` streams
without loading it all.

`rename(2)` must be atomic, including "overwrite existing target". Since every
mutation produces a new snapshot and the volume advances one root pointer
atomically, **atomicity is inherited**: build a new snapshot with the entry
removed from source, added to target, target's old inode's `nlink` decremented,
then advance the root once. There is no window in which both or neither exists.
Getting rename atomicity nearly free is the main payoff of the CIP-004 design.

### Path resolution and caching

Resolution walks the dentry HAMT per component. Every layer is content-
addressed and immutable, so **caching is trivially safe**: a node's contents can
never change under a given hash. Cache HAMT nodes and inodes by hash in a
bounded LRU (default 256 MB); invalidation happens only when the root advances,
and even then only along the changed path.

Metadata operations hit this cache, not the network. `stat` on a hot path is a
hash lookup in memory. This is what makes the mount feel like a filesystem
rather than a network protocol, and it is why the design puts metadata in a
snapshot rather than fetching per-inode records.

### Permissions

Standard POSIX mode/uid/gid checks, enforced in the FUSE layer (CIP-009) with
`default_permissions`. uid/gid are stored as written; there is no network-wide
identity mapping, so a volume mounted on two machines with different uid
namespaces sees different owners. Documented, with a `-o uid=,gid=` squash
option for the common single-user case.

## Acceptance criteria

1. `pjdfstest` POSIX conformance suite passes for `chmod`, `chown`, `link`,
   `symlink`, `mkdir`, `rmdir`, `rename`, `truncate`, `unlink` (write-path
   durability tests deferred to CIP-008).
2. A 4 KiB file writes zero shards and zero manifests; it round-trips from the
   snapshot alone.
3. Writing 1 byte at offset 0 of a 1 GiB file uploads one chunk, not 1 GiB
   (asserted via a bytes-uploaded counter).
4. Appending 1 MiB to a 10 GiB file re-uploads under 20 MiB (FastCDC working).
5. `rename` over an existing file is atomic under `kill -9` in a loop: the
   target is always either the old or the new inode, never missing.
6. A directory with 1M entries: `readdir` streams in constant memory; creating
   one more entry writes fewer than 10 HAMT nodes.
7. `stat` on a cached path completes in under 100 µs with no network I/O.
8. A sparse 1 TiB file with 1 MiB written consumes ~1 MiB.
9. `cargo test -p c0mpute-fs` covers extent splice, truncate, and inline
   promotion/demotion across the threshold.

## Risks

- **POSIX is a large surface with sharp edges.** `rename` over a non-empty
  directory, `unlink` of an open file (must stay readable until close), `nlink`
  accounting for hard links. *Mitigation:* `pjdfstest` from the first week, not
  at the end; treat it as the definition of done.
- **Unlink-while-open.** POSIX requires the inode to survive until the last
  descriptor closes. Needs an orphan list in the volume so a crash mid-open
  doesn't leak. *Mitigation:* explicit orphan inode set in the snapshot, swept
  at mount.
- **Cache coherence across mounts.** Safe here only because this CIP assumes a
  single writer; CIP-010 is where this gets hard.
- **FastCDC adds CPU on the write path.** ~1 GB/s/core for a rolling hash is
  fine for network-bound writes but not free on a node also running inference.
  *Mitigation:* benchmark; make chunking parameters and enablement per-volume.

## Estimate

**4–6 weeks.** ~1 week inode model and HAMT integration, 1 week extent trees
and splice logic, 0.5 week directories and rename, 1 week FastCDC and the
inline/extent threshold, 0.5 week caching, 1–2 weeks `pjdfstest` conformance
(assume this expands).

## Out of scope

- `O_DIRECT` — no meaningful semantics over a caching network filesystem.
- Shared writable `mmap`. Private read-only `mmap` works.
- Mandatory locking, `flock` across mounts (CIP-010 covers advisory leases).
- Quotas per directory. Volume-level accounting only.
- Case-insensitive lookup.

## Open questions

- `inline_max` of 64 KiB: bigger inlines make snapshots larger, which are
  rewritten on every metadata change. 16 KiB may reconcile better with the HAMT
  node size. Needs measurement on a real corpus.
- Should `ino` be a hash of the path (stable across snapshots, breaks on
  rename) or a counter (stable across rename, needs allocation state)? Counter
  is proposed; POSIX callers assume inode stability across rename.
- Do xattrs belong inline in the inode, or as a separate object above a size?
