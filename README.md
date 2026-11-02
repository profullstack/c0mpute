# c0mpute

> Decentralized compute network. CLI-first. Five modules out of the box:
> **transcode** (FFmpeg), **tts** (Piper / Kokoro / Pocket), **stt** (faster-whisper),
> **coinpay** (DID + escrow), and **infernet** (AI inference).

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92%2B-orange.svg?logo=rust)](https://www.rust-lang.org/)
[![Bun](https://img.shields.io/badge/bun-1.3%2B-fbf0df.svg?logo=bun)](https://bun.sh)
[![GitHub stars](https://img.shields.io/github/stars/profullstack/c0mpute?style=social)](https://github.com/profullstack/c0mpute)
[![GitHub last commit](https://img.shields.io/github/last-commit/profullstack/c0mpute)](https://github.com/profullstack/c0mpute/commits/master)
[![GitHub issues](https://img.shields.io/github/issues/profullstack/c0mpute)](https://github.com/profullstack/c0mpute/issues)
[![Nodes online](https://img.shields.io/badge/dynamic/json?url=https%3A%2F%2Fc0mpute.com%2Fapi%2Fnodes&query=%24.workers_online&label=nodes%20online&color=brightgreen)](https://c0mpute.com/nodes)
[![Goal](https://img.shields.io/badge/goal-100%2C000%20nodes-blue)](https://c0mpute.com/providers)

## Run a node

```sh
curl -fsSL https://c0mpute.com/install.sh | sh
```

That is the whole setup. The installer puts `c0mpute`, `infernet` and `coinpay` in
`~/.c0mpute/bin` and starts the worker for you (a systemd service where it can, a
background daemon otherwise). Check it with `c0mpute worker status`, then run
`c0mpute bench` once so your node gets a score on the
[node board](https://c0mpute.com/nodes).

- **Runs on:** Linux (x86_64, arm64) and macOS (Intel, Apple Silicon); Windows inside WSL2.
  Any CPU works; NVIDIA, AMD and Apple GPUs are detected and advertised.
- **Network:** outbound internet is enough to join. No account, no port forwarding.
- **What it does:** serves AI inference (through [Infernet](https://infernetprotocol.com)),
  FFmpeg transcoding and storage jobs from the network.
- **What you get:** paid inference is billed through CoinPay (16 coins or PayPal) and you set
  your payout address with `infernet payout set <coin> <address>`. Paid demand is early, so
  expect small, irregular earnings for now.
- **House models (on by default):** the node pulls and serves the models Infernet publishes at
  [infernetprotocol.com/house-models.json](https://infernetprotocol.com/house-models.json) that fit
  it (RAM, and always leaving 10% of the disk free), today Profullstack's
  `saas-business-strategist-agent` and the uncensored `huihui_ai/qwen3.5-abliterated:4b`/`:9b`.
  `c0mpute worker status` lists them and the disk they use. Opt out at install with
  `--no-house-models` (uncensored only: `--no-uncensored-models`), or later with
  `infernet house-models off` / `infernet house-models uncensored off`; cap the disk with
  `infernet house-models cap <gb>`. Models you pulled yourself are never removed.
  Details: [infernetprotocol.com/docs/house-models](https://infernetprotocol.com/docs/house-models).
- **Invite people:** share `https://c0mpute.com/r/<your-handle>`. Nodes installed from it
  show your handle on the board (`C0MPUTE_REF=<handle>` does the same by hand).

**Why:** the goal is 100,000 nodes. That much pooled capacity is enough to train and serve
open-weight models in the open, on hardware owned by the people running it. The live count
on the board says how far we are from that.

## Install

```sh
curl -fsSL https://c0mpute.com/install.sh | sh
```

That installs three CLIs into `~/.c0mpute/bin`:

| Binary | Purpose | Source |
|---|---|---|
| `c0mpute` | Umbrella CLI: jobs, workers, modules, dispatch | this repo |
| `coinpay` | DID, wallet, escrow, payments, receipts, reputation | upstream coinpay project |
| `infernet` | AI inference workload runner | [infernet-protocol](https://github.com/infernetprotocol/infernet-protocol) (upstream) |

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
c0mpute transcode submit https://example.com/input.mp4 --preset video-1080p --max-price 1.25
c0mpute infernet chat --model qwen2.5:7b "hello"

# free local CPU speech models + P2P speech worker roles
c0mpute tts setup --engine all      # Kokoro, Piper, and Pocket; models install per user
c0mpute stt setup                   # multilingual faster-whisper base, CPU int8
c0mpute worker start --roles tts,stt

# infernet: see what serves what, reserve a node
c0mpute pool qwen                        # live nodes per model
c0mpute infernet model list --node <id>  # one node's advertised models
c0mpute infernet reservation pricing     # managed endpoints, by GPU class

# webhooks: paid per delivery, sealed to the one worker that sends it (DIP-0028)
c0mpute webhook serve --price 0.001      # earn per delivery
c0mpute webhook send https://example.com/hook -d '{"event":"paid"}' --secret whsec_...

# monitor
c0mpute job status <job-id>
c0mpute tui                              # interactive dashboard (hqtui)
c0mpute doctor                           # full-stack health check
c0mpute bench                            # measure this node; the worker advertises the score

# trust
c0mpute coinpay reputation inspect did:coinpay:worker:abc123
```

The plugin form mirrors the URL namespace: `c0mpute.com/transcode`,
`c0mpute.com/coinpay`, `c0mpute.com/infernet`.

### CPU speech inference

TTS (`tts.synthesize`) supports Kokoro, Piper, and Pocket on CPU. Pocket uses
Kyutai's small 1.9M-parameter English checkpoint and eight preset voices; on Linux,
setup selects CPU-only PyTorch wheels. STT
(`stt.transcribe`) uses multilingual faster-whisper `base` with CPU int8. A normal
worker start installs the speech engines and STT on first run, then advertises
both roles automatically:

```sh
c0mpute worker start
```

Pass `--roles` to choose an explicit role set. For example,
`c0mpute worker start --roles storage` skips speech setup. Speech inference
limits its CPU threads to the node's container quota when one is available.

The Kokoro model files are Apache-2.0. Pocket TTS model weights are CC-BY-4.0;
the selected Pocket preset voices are CC0, except Alba and Anna, which are
CC-BY-4.0. Piper's runtime is GPL-3.0 and its
individual voice files carry their own licenses; the bundled Joe voice is
CC0 and Norman is public domain. faster-whisper is MIT. Agent voice presets
are based on their public OpenProfiles and can be listed with
`c0mpute tts voices`.

Applications reach the P2P speech jobs through an authenticated buyer bridge.
Run `C0MPUTE_SPEECH_TOKEN=<random-secret> c0mpute speech serve` behind a
private network or HTTPS reverse proxy, then configure the app server with
`C0MPUTE_SPEECH_URL` and the same `C0MPUTE_SPEECH_TOKEN`. The bridge defaults
to a $0 maximum bid so speech stays free; set `C0MPUTE_SPEECH_MAX_PRICE_USD`
above zero only when you explicitly want to pay workers that charge for jobs.

### Webhook workflow steps (`workflow.step`, for meshhook)

meshhook runs each step of a webhook workflow on c0mpute workers
([DIP 0030](dips/0030-workflow-step-and-jobs-bridge.md)). A step is either a
`transform` (a JMESPath expression applied to the step input) or an
`http_call` (one outbound request the requester has fully resolved). No code
from the network runs, and `http_call` only reaches public addresses: every
resolved IP is vetted, the connection is pinned to it, redirects (at most 5)
are re-vetted, bodies are capped at 1 MiB and calls at 30 s.

Workers take steps by default (`c0mpute worker start`); the role is
`workflow-step`, so `c0mpute worker start --roles storage,tts,stt` opts out.
Applications reach the network through an authenticated bridge:

```sh
C0MPUTE_JOBS_TOKEN=<random-24+-char-secret> c0mpute jobs serve --listen 127.0.0.1:8788 --max-price 0

curl -s http://127.0.0.1:8788/v1/jobs -H "Authorization: Bearer $C0MPUTE_JOBS_TOKEN" -d '{
  "workload": "workflow.step",
  "spec": {"v": 1, "kind": "transform", "config": {"expression": "items[].name"},
           "input": {"items": [{"name": "a"}, {"name": "b"}]}}
}'
# {"job_id":"…","worker":"12D3KooW…","price":0.0,"output_hash":"…",
#  "result":{"v":1,"ok":true,"output":["a","b"],"duration_ms":0}}
```

Step payloads (inputs, URLs, headers, bodies) are visible to the worker that
runs them. Keep secrets out of steps, or pin trusted workers with
`allow_peers` per request or `C0MPUTE_JOBS_ALLOW_PEERS=peerA,peerB` on the
bridge.

### Run the worker in the background

```sh
# quick, self-managed daemon: detaches, writes ~/.local/share/c0mpute/worker.{pid,log}
c0mpute worker start -d
c0mpute worker status
c0mpute worker stop

# or start attached and watch the log live, then press Ctrl-D (or Ctrl-C) to
# detach — the worker keeps running in the background. Re-running -a while a
# worker is already up just re-attaches to it.
c0mpute worker start -a
```

### Private mode: only your own machines

Not everyone wants to sell their box to the open market. A private node runs
the same workloads but only for you:

```sh
c0mpute private on                    # or: c0mpute worker start --private, or C0MPUTE_PRIVATE=1
c0mpute private                       # mode, this machine's identities, allowed buyers
c0mpute private allow 12D3KooW...     # let another of your machines hire this node
c0mpute private deny 12D3KooW...
c0mpute private off                   # back to the open market
```

In private mode the worker:

- bids only on jobs whose **signed gossip publisher** is an identity on this
  machine (`c0mpute chat`, `infernet text`, `webhook send` from the same box) or
  a peer-id you allowed. The offer body's buyer field is not trusted on its own,
  so nobody can claim to be you;
- stops publishing its capability ad, so it is not on the public status page;
- does not register with Infernet as a public provider or RPC slice.

It is stored as `[private] enabled / buyers` in `config.toml`; restart the
worker to apply a change.

What it does not hide: job offers still travel over the network's gossip
topics, and an offer carries its spec inline. Private mode decides who can
*hire* your node, not who can *see* the offers your own buyers publish. Keep
secrets out of job specs, or talk to your node directly.

### Distributed inference ("Distribute across all nodes") — automatic

The worker automatically makes every node count toward infernet's distributed
inference (IPIP-0033) — no roles, no config, no per-node bookkeeping. On
`worker start` (and every ~5 min) it:

1. lists the models on the node (`infernet model list`);
2. for each, resolves its GGUF (the Ollama blob is world-readable) and
   **registers the node as an RPC primary** for it **and** runs an `rpc-server`
   **slice** for it;
3. builds llama.cpp (`rpc-server` + `llama-server`) in the background on first
   run, auto-installing the toolchain (cmake/compiler/git via the system
   package manager).

Every node runs identical logic and advertises as both primary and slice for its
models; the control plane picks a primary + slices per request, so it scales to
any number of nodes with nobody assigning masters/slaves. Push a model to your
nodes and "Distribute across all nodes" lights up on its own once ≥2 have it.
(First-run build needs `sudo` for the toolchain install; watch
`~/.c0mpute/llama-build.log`. `C0MPUTE_RPC_MODELS="id,…"` can add models a node
hasn't pulled.)

### Run the worker as a service

For a long-running node, prefer systemd (restart-on-crash, journald logs).
A ready-to-use **user** unit ships at
[`scripts/systemd/c0mpute-worker.service`](scripts/systemd/c0mpute-worker.service):

```sh
mkdir -p ~/.config/systemd/user
cp scripts/systemd/c0mpute-worker.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now c0mpute-worker
loginctl enable-linger "$USER"        # survive logout / reboot
journalctl --user -u c0mpute-worker -f
```

Under systemd the worker runs in the foreground (no `-d`) — systemd owns the
lifecycle. See the unit header for GPU/role customization via `systemctl --user edit`.

## Architecture at a glance

```
┌──────────────────── c0mpute (Rust binary) ────────────────────┐
│   subcommands: doctor, bench, worker, job, modules, tui       │
│   plugins: transcode (in-process)                             │
│            tts / stt (local CPU model runners)                │
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
| TUI (`apps/tui`) | Bun + @profullstack/hqtui | `c0mpute tui` interactive dashboard |
| Future GPU kernels | Mojo | When a workload needs custom GPU compute (DIP-0009) |

## Repo layout

```
.
├── docs/
│   ├── protocol/                      # the wire protocol — what interop requires
│   ├── c0mpute-v1.md                  # v1 PRD (superseded where it conflicts with v2)
│   └── prds/                          # CIPs — per-phase PRDs (see prds/README.md)
├── dips/                              # design proposals (the "why")
├── node/
│   └── crates/                        # all Rust source — host + transcode workload
│       ├── c0mpute-cli/                 # produces `c0mpute`
│       ├── c0mpute-core/, c0mpute-net/, c0mpute-store/, c0mpute-gateway/
│       ├── c0mpute-verify/, c0mpute-update/, c0mpute-doctor/, c0mpute-bench/
│       ├── c0mpute-proto/, c0mpute-api/
│       ├── c0mpute-envelope/            # canonical JSON, DIDs, signed envelopes
│       └── c0mpute-transcode/           # in-process FFmpeg workload
├── plugins/                           # marketplace manifests only
│   ├── transcode/module.toml          # in-process; code at node/crates/c0mpute-transcode
│   ├── workflow-step/module.toml      # in-process; code in c0mpute-core (workflow.rs, jobs_gateway.rs)
│   ├── coinpay/module.toml            # subprocess; binary from upstream coinpay
│   └── infernet/module.toml           # subprocess; binary from infernetprotocol/infernet-protocol
├── apps/
│   ├── web/                           # @c0mpute/web — Next.js apex landing
│   └── tui/                           # @c0mpute/tui — hqtui terminal dashboard
├── packages/
│   └── shared/                        # @c0mpute/shared — shared TS types
├── .mise.toml                         # contributor toolchain pins
├── railpack.json                      # Railway build config (provider hint)
└── scripts/
    ├── install.sh                     # served at c0mpute.com/install.sh
    └── dev-setup.sh                   # contributor bootstrap
```

There is **no central backend** — no Supabase, no coordinator daemon.
Discovery and dispatch flow through libp2p Kad-DHT + gossipsub. The only
public infrastructure we host is static (landing site, release tarballs,
bootstrap seed list, plugin manifest mirrors). See
[DIP-0011](dips/0011-no-central-backend.md).

Every protocol record — provider adverts, job manifests, offers, receipts —
carries **its own signature**, so it stays verifiable after it leaves the
wire: relayed by an indexer, read back off disk, or shown to a third party
who was never a peer. Identity is a locally generated `did:c0mpute:z…`
needing no account and no payment product; CoinPay remains the default
settlement adapter and the first-party integration, named on the wire
rather than assumed. Indexers and gateways may cache, proxy and bill —
none may be authoritative.

> **If c0mpute.com disappears, c0mpute keeps computing.**

See [`docs/protocol/`](docs/protocol/README.md) for the specification,
[DIP-0024](dips/v2.x/0024-protocol-vs-hosted-services.md) for the rule that
keeps hosted services optional, and
[DIP-0025](dips/v2.x/0025-signed-envelopes-and-native-identity.md) for
identity and envelopes.

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
- `c0mpute bench` runs fib / matmul / blake3 at 1…N threads, writes
  `bench.json` (same `metadata` + `results` layout as
  [fleetcode's runtime-benchmarks](https://fleetcode.com/runtime-benchmarks/))
  and folds it into one score; the worker publishes `bench_score` in its
  capability ad and the status aggregator sums it as network capacity
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

[MIT](LICENSE)
