---
dip: 0005
title: "Rebrand to c0mpute.com; three-CLI architecture; transcode is a module"
status: Accepted
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-05-03
discussion: docs/c0mpute-v1.md
implementation:
supersedes: 0001, 0002
superseded-by:
---

## Summary

The project rebrands from depin.quest/video (Quest) to **c0mpute.com**, a
generic decentralized compute marketplace. The video product becomes the
**transcode module** — one of three v1 modules alongside **infernet**
(AI inference) and **coinpay** (DID + payments + reputation).

User-visible CLI:

```
c0mpute   — generic p2p compute network CLI (umbrella)
coinpay   — DID, wallet, escrow, payments, receipts, staking, reputation
infernet  — AI inference workload CLI/protocol
```

All three install together by default:

```bash
curl -fsSL https://c0mpute.com/install.sh | sh
```

Install layout:

```
~/.c0mpute/bin/c0mpute
~/.c0mpute/bin/coinpay
~/.c0mpute/bin/infernet
```

## Motivation

We were aiming the work at depin.quest/video as a single video product.
Spending a week on the architecture revealed:

1. The same network shape (workers + buyers + validators + escrow + DID +
   reputation) handles AI inference equally well, and inference has
   stronger commercial pull right now than video.
2. CoinPay is a reusable trust spine that other Profullstack marketplaces
   want too — making it a `depin video` subcommand under-sells it.
3. Infernet Protocol already exists and has its own brand and CLI.
   Subordinating it to a `depin` namespace creates conflict, not clarity.
4. "depin.quest/video" reads as a single product. "c0mpute.com" reads as
   infrastructure other things plug into. The latter is what we're
   actually building.

So we move the parent brand to c0mpute.com, fold Quest's domain expertise
in as the ffmpeg-transcode module, and treat coinpay + infernet as
first-class peer modules rather than internal subcommands.

## Detailed design

### Brand & domain

- Public website + dashboard: `c0mpute.com`.
- Default escrow-and-payments brand: `coinpay` (separate website at
  `coinpay.com` likely; out of scope here).
- Inference brand: `infernet` (existing, owned upstream).

### Per-module namespace (URL + CLI symmetry)

Both URL routes and CLI commands are organized by **module id**. Today's
v1 modules are `transcode`, `coinpay`, `infernet`.

| Surface | Pattern | Example |
|---|---|---|
| Web/PWA | `c0mpute.com/<module>/...` | `c0mpute.com/transcode/install` |
| CLI | `c0mpute <module> <subcommand>` | `c0mpute coinpay did create` |
| API | `c0mpute.com/api/v1/<module>/...` | `c0mpute.com/api/v1/transcode/jobs` |
| Direct module CLI | `<module> <subcommand>` | `coinpay did create` |

Each module's web surface is its own Next.js app with its own
`basePath`. Today's `apps/web` is the transcode module's web/PWA at
`/transcode`. Future `apps/coinpay-web` and `apps/infernet-web` will
mount at `/coinpay` and `/infernet` respectively. The `c0mpute.com`
apex hosts marketing + the cross-module console; that's a separate
surface (likely `apps/console` when we build it).

The c0mpute CLI mirrors the same shape: `c0mpute <plugin> <args>`. For
peer-binary modules (coinpay, infernet) the CLI uses clap's
`trailing_var_arg` to forward arguments verbatim to the underlying
binary on PATH. For in-process modules (transcode) the subcommands are
defined inline. This makes adding a new module a one-clap-variant
operation.

### Surfaces per module (web / PWA / desktop / CLI)

Long-term each module ships across all four surfaces:

- **CLI** — `c0mpute <module> ...` umbrella + the standalone `<module>`
  binary. Already exists for v1.
- **Web / PWA** — Next.js app with `basePath = "/<module>"`. Service
  worker + manifest provide installability. Today: `apps/web` (transcode).
- **Desktop (Electron)** — wraps the same Next.js bundle in a native
  window for a downloadable app on macOS/Linux/Windows. Future:
  `apps/desktop` (probably one Electron shell that loads any installed
  module's web app).

The web app and the desktop app share the Next.js source — Electron
mounts the same routes against a local HTTP server. Per-module web
apps can be hosted standalone or as part of c0mpute.com.

### Repo

This repo (currently `~/src/p2p-one`) becomes the c0mpute monorepo.
Suggested rename: `~/src/c0mpute`. The monorepo houses all three CLIs and
the dashboard for v1.

If we ever need to split, the natural fault lines are:
- `coinpay/` — once it serves multiple marketplaces, becomes its own repo.
- `infernet/` integration — likely splits the moment Infernet Protocol's
  upstream repo is the canonical source.
- `c0mpute/` core — stays in the marketplace repo.

For v1: monorepo. We don't pay the split cost until we have to.

### Cargo workspace

Three bin crates under `node/crates/`:

```
crates/
├── c0mpute-cli/      # produces `c0mpute` binary
├── coinpay-cli/      # produces `coinpay` binary
├── infernet-cli/     # produces `infernet` binary (delegates to upstream when available)
├── c0mpute-core/     # shared: job manifest types, scheduler client, etc.
├── coinpay-sdk/      # DID + signed-request envelope + escrow client
└── (existing quest-* crates retained as transcode-module internals)
```

Existing `quest-*` crates (`c0mpute-store`, `c0mpute-transcode`, `c0mpute-net`,
etc.) keep their names internally — they're the transcode module's
implementation, not a user-visible brand.

### CLI subcommand surface

`c0mpute` (top level):

```
c0mpute doctor
c0mpute worker register | start | status | stop
c0mpute job submit <manifest.json> | status <id> | logs <id> | cancel <id>
c0mpute transcode <input> [--preset ...]      # delegates to transcode module
c0mpute infer <prompts> [--model ...]         # delegates to infernet
c0mpute trust inspect <did>
c0mpute version
```

`coinpay`:

```
coinpay did create | status | export | import
coinpay wallet status | link
coinpay escrow status | create | release
coinpay receipts list
coinpay reputation inspect <did>
```

`infernet`:

```
infernet doctor
infernet run <prompts> --model <name> [--network c0mpute]
infernet models list
infernet benchmark --model <name>
```

### Install & directory layout

- Binaries: `~/.c0mpute/bin/{c0mpute,coinpay,infernet}`.
- Config: `~/.config/c0mpute/{c0mpute,coinpay,infernet}.toml` (XDG).
- Data: `~/.local/share/c0mpute/...`.
- Identity: `coinpay did create` writes `~/.config/coinpay/identity.key`.
  `c0mpute` and `infernet` read identity through `coinpay` (subprocess or
  shared SDK), never hold their own keys.

### Installer flags (per PRD §"Installer Modes")

```
--minimal       # c0mpute only
--no-coinpay
--no-infernet
--worker        # adds Docker/FFmpeg readiness checks
--developer     # adds verbose logging + dev tools
--force         # reinstall over existing
```

### Marketing

Old `apps/web` (Next.js dashboard with `basePath: /video`) gets reframed
as `c0mpute.com` itself — basePath drops back to `/`. The transcode
product page becomes one of several module pages.

## Alternatives considered

**Single `c0mpute` binary with `c0mpute coinpay ...` and
`c0mpute infernet ...` subcommands.** Closer to what DIP-0002 proposed.
Rejected: CoinPay is meant to ship across multiple marketplaces — making
it a c0mpute subcommand pins its identity to c0mpute. Same logic for
Infernet which has external upstream.

**Subdomains (`coinpay.c0mpute.com`, `infernet.c0mpute.com`).** Marketing
flexibility but every actor is a peer of c0mpute.com, not a subordinate.
Better to have first-class brands.

**Separate repos from day one.** Splits a small team across three repos
prematurely. Hold the line on monorepo until growth forces the split.

**Keep depin.quest/video as the brand.** Doesn't survive the realization
that we're building infrastructure for multiple workload types, not one
video product.

## Migration & rollout

This is a pre-launch rebrand — no real users to migrate. Concrete steps:

1. Save c0mpute v1 PRD at `docs/c0mpute-v1.md`. Mark old `docs/PRD.md` as
   superseded for v1 architecture (transcode-module-internal details
   carry forward). ✅
2. Mark DIPs 0001, 0002, 0003 superseded (this DIP plus 0006, 0007). ✅
3. Rename binary `depin` → `c0mpute` in the existing CLI crate.
4. Add `coinpay-cli` and `infernet-cli` bin crates with command-stub
   skeletons.
5. Replace `scripts/install.sh` with a script that installs all three.
6. Update `apps/web` to drop the `basePath: "/video"` and rebrand.
7. Update README + dev-setup script to reference the new CLI names.

The `quest-*` Rust crates **keep their names** — they're the transcode
module's internals, not user-visible. Renaming them to `c0mpute-transcode-*`
or `transcode-module-*` is busywork and we already pay the cost of
whatever name confusion exists once.

## Open questions

- **Repo path.** Rename `~/src/p2p-one` → `~/src/c0mpute` on disk, or
  leave the path alone and only change project naming inside? (Pending
  user confirmation — mechanical operation.)
- **Marketing site for c0mpute.com.** Is the existing `apps/web` enough,
  or do we want a separate marketing surface? Probably one Next.js app
  for v1.
- ~~**Repo URL on GitHub.**~~ Resolved: `github.com/profullstack/c0mpute`.

## Out of scope

- The actual implementation of CoinPay DID — covered in DIP-0007.
- The plugin/module discovery & dispatch contract — covered in DIP-0006.
- Whether transcode runs in a Docker sandbox or directly — out of scope;
  a Phase-3 decision.
