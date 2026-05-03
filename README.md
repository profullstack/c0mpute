# c0mpute

> Decentralized compute network. CLI-first. Three modules out of the box:
> **transcode** (FFmpeg), **coinpay** (DID + escrow), **infernet** (AI inference).

[![License](https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.92%2B-orange.svg?logo=rust)](https://www.rust-lang.org/)
[![Bun](https://img.shields.io/badge/bun-1.3%2B-fbf0df.svg?logo=bun)](https://bun.sh)
[![GitHub stars](https://img.shields.io/github/stars/profullstack/c0mpute?style=social)](https://github.com/profullstack/c0mpute)
[![GitHub last commit](https://img.shields.io/github/last-commit/profullstack/c0mpute)](https://github.com/profullstack/c0mpute/commits/master)
[![GitHub issues](https://img.shields.io/github/issues/profullstack/c0mpute)](https://github.com/profullstack/c0mpute/issues)

## Install

```sh
curl -fsSL https://c0mpute.com/install.sh | sh
```

That installs three CLIs into `~/.c0mpute/bin`:

| Binary | Purpose | Source |
|---|---|---|
| `c0mpute` | Umbrella CLI: jobs, workers, modules, dispatch | this repo |
| `coinpay` | DID, wallet, escrow, payments, receipts, reputation | upstream coinpay project |
| `infernet` | AI inference workload runner | [infernet-protocol](https://github.com/PerryTS/infernet-protocol) (upstream) |

The c0mpute installer pulls each from its own release feed.

## What c0mpute does

```sh
# identity
c0mpute coinpay did create
c0mpute coinpay did create --role worker

# run a worker
c0mpute worker register
c0mpute worker start --gpu

# submit jobs
c0mpute transcode submit input.mov --preset hls --max-price 1.25
c0mpute infernet run prompts.jsonl --model qwen --max-price 5.00

# monitor
c0mpute job status <job-id>
c0mpute tui                              # interactive dashboard (react-blessed)
c0mpute doctor                           # full-stack health check

# trust
c0mpute coinpay reputation inspect did:coinpay:worker:abc123
```

The plugin form mirrors the URL namespace: `c0mpute.com/transcode`,
`c0mpute.com/coinpay`, `c0mpute.com/infernet`.

## Architecture at a glance

```
┌──────────────────── c0mpute (Rust binary) ────────────────────┐
│   subcommands: doctor, worker, job, modules, tui              │
│   plugins: transcode (in-process)                             │
│            coinpay   (subprocess → external `coinpay` binary) │
│            infernet  (subprocess → external `infernet` binary)│
└───────────────────────────────────────────────────────────────┘
                  ▲                      ▲
                  │ libp2p Kad-DHT       │ signed-request envelopes
                  ▼                      ▼
       p2p mesh of workers          coinpay (DID, escrow, receipts)
       (no central coordinator)
```

| Layer | Language | Why |
|---|---|---|
| CLI binaries (`c0mpute`) | Rust | Static binary; no runtime to install on workers |
| P2P / chunks / FFmpeg | Rust | rust-libp2p, content-addressed storage, no GC pauses |
| Web (`apps/web`) | Bun + Next.js 16 | Apex landing at c0mpute.com |
| TUI (`apps/tui`) | Bun + react-blessed | `c0mpute tui` interactive dashboard |
| Future GPU kernels | Mojo | When a workload needs custom GPU compute (DIP-0009) |

## Repo layout

```
.
├── docs/
│   ├── c0mpute-v1.md                  # v1 PRD (current source of truth)
│   └── PRD.md                         # original Quest PRD (transcode internals)
├── dips/                              # design proposals
├── node/
│   └── crates/                        # all Rust source — host + transcode workload
│       ├── c0mpute-cli/                 # produces `c0mpute`
│       ├── c0mpute-core/, c0mpute-net/, c0mpute-store/, c0mpute-gateway/
│       ├── c0mpute-verify/, c0mpute-update/, c0mpute-doctor/
│       ├── c0mpute-proto/, c0mpute-api/
│       └── c0mpute-transcode/           # in-process FFmpeg workload
├── plugins/                           # marketplace wrappers ONLY (no source)
│   ├── transcode/{module.toml,web/}
│   ├── coinpay/{module.toml,web/}     # source lives upstream at coinpay
│   └── infernet/{module.toml,web/}    # source lives upstream at infernet-protocol
├── apps/
│   ├── web/                           # @c0mpute/web — Next.js apex landing
│   └── tui/                           # @c0mpute/tui — react-blessed TUI
├── packages/
│   └── shared/                        # @c0mpute/shared — shared TS types
├── .mise.toml                         # contributor toolchain pins
├── railpack.json                      # Railway build config (provider hint)
└── scripts/
    ├── install.sh                     # served at c0mpute.com/install.sh
    └── dev-setup.sh                   # contributor bootstrap
```

There is **no central backend** — no Supabase, no coordinator daemon.
Discovery, dispatch, and verification flow through libp2p Kad-DHT +
gossipsub. Identity, payments, escrow, and reputation flow through
CoinPay DID. The only public infrastructure we host is static
(landing site, release tarballs, bootstrap seed list, plugin manifest
mirrors). See [DIP-0011](dips/0011-no-central-backend.md).

`plugins/` directory is for **marketplace metadata only**. Each plugin's
`module.toml` describes how `c0mpute` discovers, dispatches to, and (in
the future) lets users install/enable/disable it. The plugin's actual
binary comes from its own release feed.

## Quickstart (contributors)

```sh
scripts/dev-setup.sh                   # mise + pinned tools + bun install
mise run cli -- doctor                 # full-stack diagnostics
mise run cli -- transcode preset list
mise run test                          # rust + tsc

# build the c0mpute binary directly
cargo build --bin c0mpute
./target/debug/c0mpute --help
```

## Status

**Working today**

- `c0mpute` Rust binary builds; clap surface for `doctor`, `worker`,
  `job`, `plugin`, `transcode`, `coinpay` (passthrough), `infernet`
  (passthrough), `tui`, `version`
- `c0mpute plugin install <url>` chain-calls upstream installers
- `c0mpute doctor` cross-checks `coinpay` and `infernet` on PATH
- Apex landing at [c0mpute.com](https://c0mpute.com) deployed via
  Railway, dark CLI-aesthetic with `/`, `/getting-started`, `/docs`,
  `/contact`, `/terms`, `/privacy`
- `www.c0mpute.com` → apex 308 redirect via `next.config.mjs`
- `apps/tui` scaffold renders a placeholder dashboard
- 12 Rust unit tests pass

**Not yet wired up**

- Real CoinPay DID generation, escrow, receipts (DIP-0007 — depends on
  upstream coinpay project shipping)
- Real Infernet runtime integration (depends on upstream infernet-protocol)
- libp2p networking (`c0mpute-net` is a trait surface today; bootstrap
  design in DIP-0010)
- Plugin marketplace UI on the dashboard (`/plugins` page coming)

## License

`MIT OR Apache-2.0` — pick whichever you prefer. Matches the Rust
ecosystem convention and our actual dependency tree.
