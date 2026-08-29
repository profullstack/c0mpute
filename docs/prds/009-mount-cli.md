---
cip: 009
title: "`c0mpute storage` CLI and the FUSE mount"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-08-29
updated: 2026-08-29
implements: DIP-0012 (`plugins/storage/module.toml` declares `cli = "c0mpute storage"`), DIP-0002 CLI structure
depends-on: 008
blocks: 010, 013
implementation:
estimate: "3–4 weeks"
---

## Summary

The product surface. A `c0mpute storage` command group, and a FUSE filesystem
so a volume appears at a path where `ls`, `cp`, `rsync`, `git`, and every other
ordinary tool just work.

## Motivation

`plugins/storage/module.toml` already declares `cli = "c0mpute storage"` and
workloads `storage.put`, `storage.get`, `storage.repair`. None of it exists:
the CLI has a `--storage` flag for a directory path and a `Role::Storage`
variant, and that is all. There is no mount code of any kind in the tree.

Everything CIPs 002–008 build is unreachable without this.

## Goals

- `c0mpute storage mount <volume> <path>` produces a working POSIX mount.
- Standard tools work unmodified.
- `/etc/fstab` and systemd automount support.
- A complete non-mount CLI for volumes, objects, providers, and diagnostics.
- Clear reporting of the state a distributed filesystem can be in.

## Non-goals

- Multi-writer safety (CIP-010). v1 mounts take an exclusive volume lease.
- Windows. Linux and macOS only, matching `module.toml`.
- NFS/SMB re-export (see Open questions).

## Design

### CLI surface

Following DIP-0002's nesting, under `c0mpute storage`:

```
VOLUMES
  volume create <name> [--tier standard|hot|critical] [--quota GB]
  volume list [--json]
  volume info <volume>
  volume destroy <volume> [--yes]
  volume snapshot <volume> [--label <text>]
  volume rollback <volume> --to <sequence|label>

MOUNTING
  mount <volume> <path> [-o opt,opt...]
  umount <path>
  mounts

OBJECTS  (no mount required; the CIP-002 API)
  put <file> [--tier T]        -> object hash
  get <hash> [-o out]
  ls <volume> [path]
  rm <volume> <path>

PROVIDING
  provide --disk <GB> [--path <dir>]     opt into the storage role
  provide status
  earnings [--month YYYY-MM] [--json]
  retire [--drain-timeout <dur>]         graceful exit (CIP-005)

DIAGNOSTICS
  status [<volume>]           journal lag, upload rate, degraded blocks
  health <volume>             per-object shard health
  repair <volume> [--object H]  force a repair pass
  recover --did D --volume V  rebuild from the DID key alone (CIP-004)
  verify <volume>             re-hash everything reachable; report mismatches
```

`provide` is the entry point for the supply side and should be the shortest
path in the product: one command, a disk budget, and an earnings estimate shown
before anything is committed (CIP-006's risk about disappointed providers).

### Mount options

```
c0mpute storage mount vol_7f3a /mnt/data -o fsync=network,cache=256M
```

| Option | Default | Meaning |
|---|---|---|
| `fsync=local\|network\|paranoid` | `network` | CIP-008's durability mode |
| `cache=<size>` | 256M | Metadata + block cache |
| `journal=<path>` | `~/data/c0mpute/journal` | Put this on the fastest disk |
| `journal_max=<size>` | 8G | Backpressure threshold |
| `ro` | off | Read-only; takes a shared lease, no journal |
| `uid=,gid=` | as-stored | Squash ownership for single-user mounts |
| `allow_other` | off | Standard FUSE semantics |
| `tier=` | volume default | Tier for newly written data |

### fstab and systemd

A `mount.c0mpute` helper installed in `/sbin` makes the ordinary syntax work:

```fstab
vol_7f3a  /mnt/data  c0mpute  _netdev,fsync=network,x-systemd.automount  0 0
```

`_netdev` is essential — it orders the mount after the network, and without it
boot hangs. The installer adds the helper and a systemd unit template; both are
covered by acceptance tests, because an fstab entry that hangs boot is a much
worse bug than a mount that fails.

### FUSE implementation

The `fuser` crate, with `c0mpute-fs` (CIP-007) behind it. The FUSE layer is
deliberately thin — translation and permission checks only, with no filesystem
logic — so the semantics stay testable without a mount.

- Multithreaded FUSE session; blocking network work off the reply threads.
- `default_permissions` so the kernel enforces mode bits.
- Kernel page cache enabled for reads; writeback cache **disabled**, since
  CIP-008's journal is the write buffer and two layers of write buffering make
  `fsync` semantics unanalysable.
- `readdirplus` to collapse `readdir` + `stat` storms (this is most of what
  makes `ls -l` on a big directory feel bad).
- `statfs` reports quota as total, journal lag in `f_bavail` pressure, and the
  fsync mode in the filesystem subtype so `mount` output shows it.

**macOS:** `fuser` needs macFUSE, which requires a kernel extension and is
increasingly awkward. Ship Linux first-class; macOS via macFUSE if present,
with FUSE-T (NFS-loopback, no kext) as the documented fallback. Do not block
the Linux release on macOS.

### Reporting distributed state

A network filesystem has states a local one does not, and hiding them is how
users get hurt:

```
$ c0mpute storage status vol_7f3a
volume     vol_7f3a  (standard, RS 10/14)
mounted    /mnt/data  fsync=network
sequence   41207  anchored 3s ago
journal    412 MB pending, 38 files, draining ~14 MB/s (ETA 29s)
objects    18,432 total
           18,401 healthy
               29 degraded   (repair queued)
                2 urgent     (repair in progress)
                0 lost
network    47 storage peers, 12 ASNs
```

`degraded` and `urgent` are normal steady-state on a churning network and
should not read as alarming. `lost` is the one that matters, and it is called
out separately by `health` with the affected paths — a user needs to know
*which files*, not just a count.

## Acceptance criteria

1. `mount`, then `cp -a /usr/share/doc <mount>/`, then `diff -r` reports no
   differences.
2. `git clone` a repo into the mount, `git status` clean, `git gc` succeeds.
3. `rsync -a --delete` in both directions converges with no errors.
4. `tar -xf` the kernel source and rebuild the file list identically.
5. An fstab entry with `_netdev` mounts on boot and, when the network is
   unavailable, **fails without hanging boot**.
6. `umount` flushes the journal and releases the lease; a subsequent mount
   elsewhere sees all data.
7. `ls -l` on a 10k-entry directory completes in under 1 second warm.
8. Killing the FUSE process leaves no stale mount; the next mount recovers per
   CIP-008.
9. `c0mpute storage provide --disk 500` onboards a node, and `earnings` shows
   an accrual within one billing interval.
10. `status` output above is accurate against an induced degraded state.
11. `volume rollback` restores a prior sequence and the mount reflects it.

## Risks

- **FUSE performance ceiling.** Context switches per operation put a floor on
  latency; metadata-heavy workloads feel slower than local disk regardless of
  our caching. *Mitigation:* `readdirplus`, aggressive metadata caching, honest
  benchmarks in the docs. Consider `io_uring`-based passthrough later.
- **Stale mounts after a crash.** The classic FUSE annoyance — a `d`
  directory that `ls` hangs on. *Mitigation:* explicit `fusermount3 -u`
  handling on startup, a `mounts --clean` subcommand, and never leaving the
  session without unmounting.
- **macFUSE friction on macOS.** *Mitigation:* FUSE-T fallback, and don't gate
  the Linux launch.
- **`_netdev` omitted in a hand-written fstab hangs boot.** *Mitigation:*
  `mount.c0mpute` refuses to mount at boot-time without `_netdev` and logs why;
  the docs lead with the correct line.
- **Users will mount over an existing non-empty directory and lose sight of the
  data underneath.** *Mitigation:* warn unless `--force`.

## Estimate

**3–4 weeks.** ~1.5 weeks the FUSE layer and its operation surface, 1 week the
CLI command group, 0.5 week fstab/systemd integration plus the mount helper,
0.5 week status/health reporting, 0.5 week macOS.

## Open questions

- NFS re-export (`fuser`'s `--export` / kernel NFS over FUSE) would give
  Windows and appliance access for free. Worth it in v1, or does it multiply
  the consistency surface before CIP-010 is settled?
- Should `mount` daemonise by default or run in the foreground? Foreground is
  better for systemd; daemonising is what people expect interactively.
- Does `volume rollback` need to be blocked while mounted, or can it be applied
  live with a cache invalidation?
