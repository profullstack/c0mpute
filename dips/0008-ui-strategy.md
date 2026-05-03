---
dip: 0008
title: "UI strategy: CLI-first, simple web landing, react-blessed TUI, Perry GUIs later"
status: Accepted
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-05-03
discussion:
implementation: apps/web (landing), apps/tui (react-blessed scaffold), Cmd::Tui in node/crates/c0mpute-cli
supersedes:
superseded-by:
---

## Summary

c0mpute's user surfaces, ranked by priority for v1:

1. **Rust CLI** (`c0mpute`, `coinpay`, `infernet`) — the primary product.
   Static commands, scripted workflows, headless workers.
2. **Simple web landing** at `c0mpute.com` — dark, terminal-aesthetic,
   single Next.js app. Just enough to explain the install + link to
   `getting-started`, `docs`, `contact`, `terms`, `privacy`.
3. **TUI** at `c0mpute tui` — react-blessed (Bun) terminal dashboard for
   interactive views (worker status, live jobs, module browser).
4. **Per-plugin web dashboards** at `c0mpute.com/transcode`,
   `c0mpute.com/coinpay`, `c0mpute.com/infernet` — **deferred**. Stubbed
   in `plugins/<id>/web/` as placeholders.
5. **Desktop / PWA / native GUIs** — also deferred. Long term we adopt
   [Perry](https://github.com/PerryTS/perry) for native cross-platform
   GUIs once their CLI surface ships and the project is stable.

## Motivation

Two failure modes we want to avoid:

- **Building a SaaS console nobody asked for.** Most providers and
  buyers will run `c0mpute` from their shell; the web stuff is a nice
  marketing surface but not the product. Pour energy into the CLI
  first.
- **Picking a UI framework before it's mature.** Perry looks promising
  but their CLI integration is "coming soon." Picking it as our v1 GUI
  framework now means either waiting on them or vendoring early-stage
  code. Neither is the right tradeoff in v1.

The middle ground is: ship the CLI, ship a simple web landing, ship a
react-blessed TUI for the interactive bits, defer everything else.

## Detailed design

### Web landing (`apps/web`)

- Single Next.js 16 app at the c0mpute apex (no `basePath`).
- Dark mode, terminal aesthetic — JetBrains Mono / SF Mono, black
  background, single green accent. CSS in `apps/web/src/app/globals.css`.
- Pages: `/`, `/getting-started`, `/docs`, `/contact`, `/terms`,
  `/privacy`. Footer links GitHub + license.
- Content: install command + brief "what is this" + pointer to docs.
  No marketing splash, no social proof, no fake testimonials.
- Per-plugin dashboards live in `plugins/<id>/web/` as stubs (README
  only) and will mount as separate Next.js apps under their plugin id
  (`/transcode`, `/coinpay`, `/infernet`) when they exist.

### TUI (`apps/tui`)

- Bun + react-blessed. Produces a binary `c0mpute-tui` (via
  `bun build --compile`).
- Launched via `c0mpute tui` — the Rust CLI subprocess-launches it the
  same way it shells out to `coinpay` / `infernet`. If `c0mpute-tui`
  isn't on PATH, the user gets the same install hint as for any other
  peer binary.
- Initial views (Phase 2):
  - Worker dashboard — live status, hardware, current job.
  - Job tail — live progress for one or many jobs.
  - Module browser — list / install / enable / disable.
  - Doctor — interactive version of `c0mpute doctor`, with auto-fix.
- The TUI talks to the same coordinator API the CLI uses; no separate
  protocol.

### Why react-blessed over alternatives

- **Ratatui (Rust)** — would let us keep everything in one Rust binary.
  Considered, but the React component model we'll reuse on the web is
  worth far more than a build-system simplification, and the
  subprocess-on-PATH model is already how every other module ships.
- **Ink (React for CLIs)** — modern, but more focused on output
  formatting than interactive dashboards. blessed is older but still
  the strongest choice for full-screen terminal UIs.
- **Plain blessed without React** — works, but our web team already
  reads / writes React. Sharing components between the TUI and the
  future web dashboards is plausible if we keep them React-flavored.

### GUI strategy (long-term, deferred)

When Perry's CLI is stable and the project has matured:

- Native cross-platform GUIs (macOS, Linux, Windows) per surface.
- Most likely: a single Perry "console" app that loads any installed
  module's UI. Mirrors the way the c0mpute CLI loads any installed
  module's commands.
- We don't build for GUIs until we have real users asking for them.

### Electron / PWA — explicit non-goal for v1

PWA and Electron desktop apps were on the roadmap two iterations ago.
Both are dropped from v1:

- PWA — adds complexity (service worker, manifest discipline) for
  marginal benefit on a CLI-first product.
- Electron — heavier still; can revisit as a Perry alternative.

## Alternatives considered

**Build everything web-first.** Conventional but wrong shape. Workers
run on headless boxes. Buyers want CI scripts. CLI is the primary
input device.

**Ratatui + skip the web entirely.** Would simplify the toolchain but
loses the marketing landing and the future GUI path. The web work for
v1 is small (one Next.js app, six pages); it's not the cost driver.

**Wait for Perry to be ready.** Possible but gambles on someone else's
schedule. react-blessed is mature, works today, and the TUI surface is
a contained scope.

## Migration & rollout

This DIP captures decisions already implemented in this session:

- `apps/web` is the c0mpute landing (no basePath).
- `apps/tui` is scaffolded with react-blessed; today it renders a
  static placeholder so the wiring works end-to-end.
- `c0mpute tui` Rust subcommand is wired and shells out to the TUI.
- `plugins/<id>/web/` directories exist with README stubs.

Phase 2 work (not in this DIP):

- Real TUI views with live data from the coordinator API.
- Per-plugin web dashboards once the network has actual jobs to display.

## Open questions

- **Single TUI binary vs per-plugin TUIs?** Right now `c0mpute tui` is
  one binary. If a plugin wants its own deeper dashboard it could ship
  `c0mpute-<plugin>-tui` discovered the same way as other peer
  binaries. Decide when a plugin actually wants this.
- **Authentication in the TUI.** Same question as the web dashboard —
  defer until DIP-0007 (CoinPay DID) is implemented.

## Out of scope

- The actual Perry framework integration (long-term).
- Mobile apps. Not happening in v1; possibly never (CLI on a phone is
  a usability dead end).
