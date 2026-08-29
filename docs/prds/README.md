# CIPs — c0mpute Improvement Protocols

A **CIP** is a product requirements document for a shippable phase of work.
Where a [DIP](../../dips/README.md) records *why we decided something*, a CIP
records *what we are building, in what order, and how we will know it works*.

The two are complements, not competitors:

| | DIP | CIP |
|---|---|---|
| Question it answers | "Why this way?" | "What ships, and when is it done?" |
| Lifetime | Permanent record of a decision | Closed when the phase ships |
| Contains | Alternatives, trade-offs, policy | Scope, API surface, acceptance criteria, estimate |
| Triggers | A durable or hard-to-undo choice | A unit of work big enough to plan |

A CIP should cite the DIP it implements. If a CIP finds that its governing DIP
is wrong, the fix is a new DIP — not a CIP that quietly contradicts one.

## When to write one

Write a CIP when a body of work is large enough that someone else would need a
plan to pick it up: more than a couple of weeks, more than one crate, or a
phase with a dependency on another phase. Small self-contained changes just get
a PR.

## Numbering & layout

Three-digit zero-padded, monotonically increasing, no gaps: `001`, `002`,
`003`. Numbers are assigned when the CIP opens a PR. Filenames are
`NNN-short-slug.md` and live flat in `docs/prds/`.

Numbering is independent of DIP numbering. CIP-007 has nothing to do with
DIP-0007.

## Lifecycle

```
Draft  →  Review  →  Approved  →  In progress  →  Shipped
                  ↘  Rejected
                  ↘  Deferred
```

- **Draft** — author is still iterating.
- **Review** — open for comment on the PR that introduces it.
- **Approved** — implementation may begin.
- **In progress** — someone is building it; the `implementation` frontmatter
  field points at the PR(s).
- **Shipped** — merged and released. Acceptance criteria are all checked.
  Don't edit a Shipped CIP except for typos.
- **Deferred** — real work, not now. Keep the doc; record what would unblock it.

## Authoring flow

1. Copy `000-template.md` to `NNN-your-slug.md` (next free NNN).
2. Fill in frontmatter and body. Every CIP needs **acceptance criteria** that
   a reviewer could actually run.
3. Open a PR titled `CIP-NNN: <title>`.
4. Move to `Approved` after review, then `Shipped` when it lands.

## Index — Storage & filesystem program

Delivering read/write network storage for c0mpute, implementing
[DIP-0012](../../dips/0012-storage-plugin.md).

| #   | Title | Depends on | Status |
|-----|-------|-----------|--------|
| [001](001-storage-program.md) | Storage program: durability model, tiers, and economics | — | Draft |
| [002](002-storage-http-api.md) | Storage HTTP API on the gateway | 001 | Draft |
| [003](003-shard-placement-transport.md) | Cross-node shard placement and streaming transport | 002 | Draft |
| [004](004-metadata-durability.md) | Metadata durability: manifests, volumes, and the root pointer | 002 | Draft |
| [005](005-repair-daemon.md) | Auto-repair daemon | 003, 004 | Draft |
| [006](006-challenges-metering-payouts.md) | Storage challenges, metering, and provider payouts | 003, 004 | Draft |
| [007](007-c0mputefs-filesystem.md) | c0mputefs: mutable filesystem over immutable content | 004 | Draft |
| [008](008-write-path-consistency.md) | Write path: chunking, journal, and crash consistency | 007 | Draft |
| [009](009-mount-cli.md) | `c0mpute storage` CLI and the FUSE mount | 008 | Draft |
| [010](010-leases-multi-mount.md) | Single-writer leases and multi-mount coherence | 009 | Draft |
| [011](011-encryption-keys.md) | Client-side encryption and key management | 007 | Draft |
| [012](012-s3-gateway.md) | S3-compatible gateway | 004 | Draft |
| [013](013-databases-on-storage.md) | Databases on c0mpute storage: what we support | 010 | Draft |

### Critical path to a read/write v1

```
001 ──► 002 ──► 003 ──► 005 ──┐
          │                    ├──► 009 ──► 010 ──► 013
          └──► 004 ──► 007 ──► 008
                  │        │
                  ├──► 012 └──► 011
                  └──► 006
```

`012` (S3 gateway) and `006` (payouts) are parallel tracks; neither blocks the
mount. `011` (encryption) can land after first mount but before public launch.

### Effort

**35–49 engineer-weeks** in total. The longest dependency chain —
001 → 002 → 004 → 007 → 008 → 009 → 010 → 013 — is **21–29 weeks**, so with two
or three engineers running the parallel tracks the calendar estimate is roughly
**5–7 months to a read/write v1**.

Ship order, if you want something usable before the whole program lands:

1. **001–003 + 005.** Durable distributed object storage. Real product, no
   filesystem.
2. **012.** S3 gateway. Depends only on 004 and gets more users than FUSE will,
   because every existing tool already speaks it.
3. **004 + 007 + 008 + 009.** The read/write mount.
4. **010 + 011 + 013.** Multi-mount safety, encryption, and the database
   support matrix — all needed before a public launch, none before an internal
   one.

`006` (payouts) gates the *supply* side rather than the product, so it has to
land before there are third-party providers to pay, not before the first mount
works.
