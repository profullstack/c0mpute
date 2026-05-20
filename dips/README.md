# DIPs — depin Improvement Plans

A DIP is a design proposal for a change that affects multiple components,
crosses a project-policy line, or is hard to undo once shipped. DIPs are
where we record the *why* behind durable decisions so future contributors
don't have to reverse-engineer them from git history.

If a change is small, local, and obvious from the diff — just open a PR. DIPs
are for the things you'd want to read about a year from now.

## When to write one

Write a DIP if any of the following are true:

- The change touches the public API surface (REST routes, CLI flags,
  on-disk config keys, P2P protocol IDs).
- The change creates or removes a product line, repo, or top-level namespace.
- The change locks in an interface other systems will integrate against
  (release manifest format, payout schema, embed contract).
- The change has plausible alternatives, and the trade-offs deserve to be
  written down before code lands.
- A reviewer asks for one.

If you're not sure, write a short one. Three paragraphs is fine.

## Lifecycle

```
Draft  →  Review  →  Accepted  →  Final
                  ↘  Rejected
                  ↘  Withdrawn
                  ↘  Superseded by DIP-NNNN
```

- **Draft** — author is still iterating; comment-friendly.
- **Review** — open for project-wide discussion. Reviewers leave comments
  inline on the PR that introduces the DIP.
- **Accepted** — implementation may begin. Status is recorded in the DIP's
  frontmatter.
- **Final** — implementation has shipped, and the DIP is now historical
  record. Don't edit a Final DIP except for typo fixes — open a follow-up
  DIP that supersedes it.
- **Rejected / Withdrawn / Superseded** — kept on disk; the *why* is part of
  the record.

## Numbering & directory layout

Four-digit zero-padded, monotonically increasing, no gaps: `0001`, `0002`,
`0003`. The number is assigned when a DIP enters Review (i.e. opens a PR).
Use the next free number — don't reserve in advance. Numbering stays
globally sequential across all directories — moving a DIP from v1.x to
v2.x scope doesn't renumber it.

The filename is `NNNN-short-slug.md`, e.g.
`0003-libp2p-protocol-versioning.md`.

DIPs are grouped by major product version. v1.x DIPs live at the top of
`dips/`. v2.x DIPs live in `dips/v2.x/`. Future major versions get their
own subdirectory (`dips/v3.x/`, etc.); v1.x stays flat for historical
reasons.

## Authoring flow

1. Copy `0000-template.md` to `NNNN-your-slug.md` (next free NNNN). Place
   it in `dips/` for v1.x scope, or `dips/v2.x/` for v2.x scope.
2. Fill in the frontmatter and body.
3. Open a PR titled `DIP-NNNN: <title>`.
4. Status starts at `Draft`. Move to `Review` when you want feedback.
5. After project consensus, change status to `Accepted` and merge.
6. When implementation lands and ships, follow up with a small PR moving
   status to `Final` and adding a "Implementation" link.

## Frontmatter fields

```yaml
---
dip: 0001
title: "Short imperative title"
status: Draft   # Draft | Review | Accepted | Final | Rejected | Withdrawn | Superseded
authors:
  - name@example.com
created: 2026-05-03
updated: 2026-05-03
discussion: <PR or issue link>
implementation: <PR link, set when status=Final>
supersedes: <DIP-NNNN if any>
superseded-by: <DIP-NNNN if this gets replaced>
---
```

## Index

### v1.x

| #    | Title                                                            | Status     |
|------|------------------------------------------------------------------|------------|
| 0001 | URL namespace under `/video`                                     | Superseded by 0005 |
| 0002 | CLI binary is `depin`, nested by line                            | Superseded by 0005 |
| 0003 | Nostr-keyed signed-request envelope for auth                     | Superseded by 0007 |
| 0004 | Pin contributor toolchain via mise                               | Accepted   |
| 0005 | Rebrand to c0mpute.com; three-CLI architecture; transcode is a module | Accepted |
| 0006 | Module model: TOML manifest + subprocess dispatch                | Accepted   |
| 0007 | CoinPay DID is the canonical identity, payment, reputation layer | Accepted   |
| 0008 | UI strategy: CLI-first, simple web, react-blessed TUI, Perry later | Accepted |
| 0009 | Mojo for GPU/kernel-shaped compute (when applicable)             | Accepted   |
| 0010 | Operator-run seed nodes for libp2p Kad-DHT bootstrap             | Accepted   |
| 0011 | No central backend; libp2p + CoinPay are source of truth         | Accepted   |
| 0012 | c0mpute is compute-only; storage is BYOS                         | Accepted   |
| 0013 | Position: GPU batch-compute marketplace; 5–8× cheaper niche      | Accepted   |
| 0014 | Public /status page + status-aggregator service                  | Accepted   |
| 0015 | Hosting vertical: censorship-resistant static sites              | Draft      |
| 0017 | Streaming reads: progressive RS reconstruction for sequential workloads | Draft |

### v2.x (`dips/v2.x/`)

| #    | Title                                                            | Status     |
|------|------------------------------------------------------------------|------------|
| 0016 | Abuse policy: CSAM hash blocklist, per-worker opt-in             | Draft      |
