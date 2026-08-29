---
cip: 013
title: "Databases on c0mpute storage: what we support"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-08-29
updated: 2026-08-29
implements: DIP-0012
depends-on: 010
blocks:
implementation:
estimate: "2–3 weeks (validation, tooling, docs — little new storage code)"
---

## Summary

Determine, by measurement rather than assertion, which database workloads run
correctly on a c0mpute mount, and ship the tooling and documentation for the
ones that do. The deliverable is a supported-configuration matrix backed by
crash tests, plus first-class support for the two patterns that work well:
**database backup/WAL archiving** and **read-replica dataset distribution**.

## Motivation

"Storage for file sharing and dbs" is the ask, and the mount is read/write, so
people will absolutely put databases on it. The question is not whether to
allow that — it is whether we find out what breaks in a test harness or in a
customer's production data.

Two facts set the boundary, and they come from earlier CIPs rather than from
caution:

1. **`fsync` costs a network round trip.** CIP-008's table puts `network` mode
   at 200–800 ms. A database that fsyncs per commit is limited to a few
   transactions per second. That is a performance fact, not a correctness one.
2. **Cross-mount locking does not exist.** CIP-010 makes `flock` mount-local
   and enforces one writer per volume. A database that relies on file locks to
   arbitrate between processes on *different* machines has no protection at
   all.

The second is the dangerous one. SQLite's reputation for corruption on network
filesystems comes almost entirely from broken locking on NFS, not from slow
I/O. Our single-writer lease actually addresses the usual cause — but only if
the deployment respects it, which is exactly what documentation and tooling
have to enforce.

## Goals

- A tested support matrix: engine, configuration, verdict, measured numbers.
- Crash-consistency validation for anything marked supported.
- First-class WAL-archive and backup tooling.
- Clear, loud guidance against the configurations that corrupt data.

## Non-goals

- Making a network filesystem competitive with local NVMe for OLTP. It is not,
  and no amount of work here changes that.
- A block-device interface (NBD). See Future work.
- Distributed/multi-writer database support of any kind.

## Design

### Support matrix (hypotheses to be confirmed by the test plan)

| Engine / config | Verdict | Reasoning |
|---|---|---|
| **SQLite, WAL mode, single mount, `fsync=network`** | Expected **supported** | One writer enforced by lease; WAL is append-heavy, which suits FastCDC; commit latency ~1 round trip |
| SQLite, rollback journal | Discouraged | fsyncs more per commit; no correctness issue, poor performance |
| SQLite, multiple mounts | **Unsupported — corrupts** | Cross-mount locking does not exist. Blocked by the lease, but must be documented |
| SQLite, `PRAGMA synchronous=OFF` | **Unsupported** | Loses durability the storage layer is providing; corruption on crash |
| **DuckDB, read-only over a dataset** | Expected **supported** | Read-mostly analytics is the ideal fit: large sequential reads, no write path |
| DuckDB, read-write | Needs measurement | Large temp/spill files; may be fine with a local spill dir |
| **Postgres, `PGDATA` on the mount** | **Unsupported** | Assumes local-disk fsync/`O_DIRECT` semantics and per-file durability guarantees a network FS cannot honour; commit latency makes it unusable regardless |
| **Postgres, base backup + WAL archive to the mount** | **Supported — recommended** | Sequential, append-only, no fsync-per-commit. The right pattern |
| MySQL/MariaDB `datadir` on the mount | **Unsupported** | Same reasons as Postgres |
| LMDB / RocksDB on the mount | Unsupported | `mmap` shared-writable (LMDB) is out of scope in CIP-007; RocksDB's fsync pattern is hostile |
| **Turso / libSQL replica sync to the mount** | Expected supported | Already an embedded-replica model; the mount just holds the file |

The two rows in bold that say "recommended" are the actual product here.
Everything else is either a measured yes or an honest no.

### Pattern 1: WAL archive and backup (recommended)

This is what most people asking for "databases on distributed storage" actually
need, and it plays to every strength of the design: append-only writes,
sequential reads, and content-addressed dedup across daily backups.

```bash
# postgresql.conf
archive_mode = on
archive_command = 'test ! -f /mnt/c0mpute/wal/%f && cp %p /mnt/c0mpute/wal/%f'
```

Ship `c0mpute storage db-backup` wrapping the common cases:

```
c0mpute storage db-backup postgres --volume vol_7f3a --dsn ... --schedule daily
c0mpute storage db-backup sqlite   --volume vol_7f3a --file app.db
c0mpute storage db-restore --volume vol_7f3a --to ./restored --at 2026-08-29T12:00Z
```

Point-in-time restore comes almost free from CIP-004's retained roots and
CIP-007's structural sharing: a daily backup of a 100 GB database that changes
1% stores ~1 GB of new chunks per day, not 100 GB. Dedup across backups is the
single most compelling storage economic in the whole product, and it should be
measured and published.

### Pattern 2: read-replica dataset distribution (recommended)

Write a database file once, mount it read-only on many workers, query in
parallel. CIP-010 gives unlimited read-only mounts with consistent
point-in-time snapshots, which is exactly the semantics a shared analytical
dataset wants.

This is the compute-locality argument made concrete: a DuckDB or SQLite dataset
read by 50 c0mpute workers pays $0 internal egress (CIP-001) where R2 or B2
would charge for every worker's read.

### Pattern 3: single-writer OLTP (supported, with numbers attached)

SQLite in WAL mode on a `fsync=network` mount, one writer. Correct, because the
lease enforces the single-writer assumption SQLite already requires. Slow, in a
way that must be quantified rather than hand-waved — the documentation should
carry the measured tps, not an adjective.

Provide a tuned mount profile:

```
c0mpute storage mount vol_7f3a /mnt/db -o fsync=network,profile=sqlite
```

which sets a smaller chunk target (WAL frames are small), disables FastCDC on
`-wal` files (their boundaries are already frame-aligned), and pins the journal
to the fastest local device.

### Test plan

The matrix above is a set of hypotheses, and this is the work that turns it
into a supported-configuration list:

1. **Crash consistency.** For each candidate config: run a write workload, cut
   power (`dm-flakey`, `kill -9`, and network partition), then run the engine's
   own integrity check (`PRAGMA integrity_check`, `pg_checksums`, `amcheck`).
   1000 iterations, zero corruptions required to earn "supported".
2. **Performance.** `pgbench`, `sqlite-bench`, and a DuckDB TPC-H subset across
   all three fsync modes, published as a table with real numbers.
3. **Lease enforcement.** Attempt the corrupting configurations deliberately —
   two mounts, one database — and confirm CIP-010 blocks them.
4. **Backup/restore.** Restore correctness at multiple points in time; measure
   dedup ratio across 30 daily backups.
5. **Long-running soak.** Two weeks of continuous write load with induced node
   churn and repair, confirming no corruption under CIP-005 activity.

Anything that fails crash consistency is documented as unsupported and, where
possible, **actively refused** — `profile=sqlite` should reject
`synchronous=OFF` rather than allowing a foot-gun.

### Guardrails

Documentation alone will not stop someone pointing `PGDATA` at the mount.

- The mount detects known database file signatures (`PGDATA/PG_VERSION`,
  `ibdata1`, LMDB `data.mdb`) appearing in an unsupported configuration and
  emits a loud warning to `status` and the system log.
- `--allow-unsupported-db` exists to silence it, because someone will have a
  good reason and refusing outright is paternalistic. But it must be a
  deliberate act, not a default.

## Acceptance criteria

1. The support matrix is published with measured numbers in every row, and no
   row says "should work" without a test behind it.
2. Every configuration marked supported survives 1000 crash iterations with
   zero integrity-check failures.
3. `db-backup postgres` + `db-restore --at <time>` restores a database that
   passes `amcheck`.
4. 30 daily backups of a 100 GB database with 1% daily change consume under
   150 GB total (dedup working).
5. Two simultaneous read-write mounts holding one SQLite database are blocked
   by the lease, with an error naming the conflict.
6. A read-only fan-out of 50 mounts over one DuckDB dataset returns identical
   query results on every worker.
7. `PGDATA` on the mount triggers the warning.
8. Published `pgbench`/`sqlite-bench` numbers for all three fsync modes.

## Risks

- **Someone runs an unsupported config and loses data, then blames the
  product.** The realistic worst case. *Mitigation:* guardrails, warnings, an
  unambiguous matrix, and refusing the worst combinations outright.
- **SQLite-on-WAL fails crash testing.** Possible; the interaction between
  SQLite's fsync expectations and CIP-008's journal is exactly what test 1
  exists to find. *Mitigation:* if it fails, mark it unsupported and say so.
  Better to ship a smaller true claim.
- **Measured performance is bad enough to be embarrassing.** Likely for OLTP.
  *Mitigation:* lead with patterns 1 and 2, which are genuinely excellent, and
  publish the OLTP numbers honestly rather than omitting them.
- **Users read "read/write mount" as "any database works".** *Mitigation:* the
  matrix is the headline of the docs page, not an appendix.

## Estimate

**2–3 weeks**, mostly validation rather than new storage code: ~1 week the
crash-test harness across engines, 0.5 week benchmarks, 0.5 week `db-backup`
/`db-restore` tooling, 0.5 week guardrails and documentation.

## Future work: block device (NBD)

The way to genuinely support Postgres is to stop pretending a network
filesystem is a disk and expose a **block device** instead: an NBD server
backed by c0mpute storage, with a local writeback cache and honest flush
semantics. Postgres formats it with a real filesystem and gets the durability
model it expects.

That is a substantial separate project — block-level caching, flush ordering,
and single-attach enforcement — and it should be its own DIP. It is the correct
answer to "run my database on c0mpute", and it is not v1.

## Open questions

- Should `profile=sqlite` be automatic when a `.db`/`-wal` pair is detected?
  Magic that helps is still magic that surprises.
- Is there demand for a managed "c0mpute Postgres" (we run it on local NVMe on
  a worker, back it up to storage) rather than users assembling it? That is a
  product question, not a storage one.
- Does Turso/libSQL embedded replica sync deserve first-class tooling given it
  is already the house pattern for several Profullstack projects?
