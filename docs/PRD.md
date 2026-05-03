# Quest — Decentralized Video Transcoding & Hosting Network

**Product Requirements Document**
**Status:** Draft v0.1
**Owner:** TBD
**Last updated:** 2026-05-03
**Repo (planned):** github.com/depinquest/quest
**Dashboard:** https://depin.quest/video

> **Namespace note:** All product surfaces — dashboard, API, install script,
> release artifacts — live under the `/video` path on `depin.quest`. The root
> domain is reserved as a parent brand for future product lines (e.g.
> `/storage`, `/compute`). Treat every URL in this document as prefixed with
> `https://depin.quest/video/...` unless otherwise noted.

---

## 1. Executive Summary

Quest is a fully open-source, decentralized network for video transcoding and hosting. Anyone with spare GPU/CPU/disk/bandwidth can run a node and earn money by transcoding customer videos and serving them to viewers. Customers upload once, get back a content-addressed `quest://` URL, and embed an HLS player that streams from the network at 50–80% lower cost than traditional CDNs.

The network is coordinated via libp2p Kad-DHT for peer/chunk discovery, replicates content with Reed-Solomon erasure coding, and pays providers in crypto via CoinPayments. A self-hostable Supabase + Next.js dashboard at `depin.quest/video` gives both providers and customers a Stripe-quality view of costs, earnings, and network health.

The core node is a single static Rust binary, installable via `curl | sh`, that self-upgrades and self-heals.

---

## 2. Vision & Strategic Positioning

**Vision:** Make the world's video infrastructure (storage, transcoding, delivery) a liquid commodity priced by supply and demand instead of by hyperscaler margin.

**Wedge:** AV1 backlog migration and creator-direct VOD. Streaming companies have petabytes of H.264 they want re-encoded to AV1 (30%+ bandwidth savings) but can't afford AWS MediaConvert pricing for the backlog. Quest underprices that workload by ~70% by routing through consumer/prosumer GPUs.

**Positioning relative to existing players:**

| Player | What they do | Why Quest is different |
|---|---|---|
| AWS MediaConvert / Mux | Hosted transcoding + CDN | We're 50–70% cheaper, FOSS, no vendor lock |
| Livepeer | Decentralized live transcoding | We focus on VOD + storage, not just live |
| Theta | P2P CDN | We do transcoding too, simpler stack |
| IPFS/Filecoin | Decentralized storage | We're optimized for video specifically |
| Bunny.net | Cheap video CDN | We're cheaper, FOSS, providers earn |

---

## 3. Target Users

### Provider personas

**P1 — Home GPU operator.** Owns a gaming PC with an RTX 4070+. Wants passive income from idle GPU cycles. Will run the CLI on their main PC or a dedicated mini-rig. Earnings target: $50–300/month.

**P2 — Prosumer rig operator.** Has 2–8 GPUs in a homelab or small datacenter. Treats this as a side business. Wants stable earnings, predictable utilization. Target: $500–5000/month.

**P3 — Datacenter operator.** Has spare capacity on existing infrastructure (mining rigs that pivoted, regional MSPs, etc). Wants enterprise-grade SLAs and bulk pricing. Target: $5K–100K+/month.

### Customer personas

**C1 — Independent creator.** YouTuber/podcaster who wants lower fees than YouTube/Vimeo and direct monetization. Uploads weekly. Cares about: simple embed, decent player, paywall support.

**C2 — VOD platform.** Mid-size streaming service (think regional Netflix-like, niche genre, training/education) doing AV1 backlog migration or running their own CDN. Cares about: API, throughput, regional availability, DRM (eventually).

**C3 — AI/data company.** Needs video format conversion, frame extraction, or training data prep at scale. Cares about: API, batch throughput, cost.

**C4 — Sovereign-content org.** Law firms, healthcare, journalists, defense. Cares about: encryption, self-hosted gateway nodes, no-data-leaves-our-network deployment.

---

## 4. Problem Statement

Video infrastructure is one of the most expensive line items for any media company. AWS CloudFront egress is ~$0.085/GB. Mux charges $0.04/min for encoding plus delivery. A mid-size VOD platform burns $50K–500K/month on Mux/Cloudflare alone.

Meanwhile, millions of consumer GPUs sit idle 80% of the time, and home broadband uploads are radically underutilized. The arbitrage exists; nobody has packaged it for video specifically with a usable developer experience.

**Existing decentralized attempts fail because:**
1. They use IPFS, which is bad for video (block size, slow lookups, no streaming).
2. They have no transcoding layer (just storage), so customers still need a separate transcoding pipeline.
3. They have terrible UX — no real dashboard, no Stripe-like billing, no embed code, no SLA story.
4. They focus on consumer-facing "decentralized YouTube" instead of B2B infra.

Quest fixes all four.

---

## 5. Product Overview

### What customers get

1. Upload a source video (drag-drop, S3 URL, or `quest upload` CLI command).
2. Quest transcodes it to multiple renditions (1080p/720p/480p H.264 + AV1) via the distributed worker pool.
3. Customer gets back a root content hash (`quest://blake3:abc123...`) and an HLS playlist URL served by gateway nodes.
4. Drop the embed code on a website — it just plays.
5. Pay per GB stored, per minute transcoded, per GB served. Top up via CoinPayments.

### What providers get

1. `curl https://depin.quest/video/install.sh | sh` installs the Quest node.
2. `quest start --gpu --storage 500GB` runs it.
3. Earnings accrue in real-time, visible in the dashboard.
4. Withdraw to any wallet supported by CoinPayments.

### What network operators (us) get

A 15–25% network fee on every job, denominated in customer currency.

---

## 6. Goals & Non-Goals

### Goals

- **G1.** Working network with ≥100 nodes serving real video by month 4.
- **G2.** Cost to customers ≤50% of Mux/CloudFront for equivalent quality.
- **G3.** Provider earnings ≥$1.50/hour for an RTX 4070-class GPU at 50% utilization.
- **G4.** P95 first-frame latency ≤2.5s for cached content; ≤6s for cold content.
- **G5.** 99.9% chunk availability for content with default replication.
- **G6.** Single static Rust binary, ≤30MB, self-upgrading.
- **G7.** Fully FOSS — every component licensed Apache-2.0 or MIT (with explicit exceptions noted in §15).

### Non-goals (v1)

- **NG1.** Live streaming — VOD only. Live is Phase 3.
- **NG2.** DRM (Widevine/PlayReady/FairPlay). We support encryption, not commercial DRM. Phase 3.
- **NG3.** Recommendation engine, social features, "decentralized YouTube." We're infrastructure, not a destination.
- **NG4.** Mobile node app. Desktop/server only in v1.
- **NG5.** Token launch. We pay in stablecoins via CoinPayments. No ICO, no governance token.
- **NG6.** On-chain settlement. Crypto payouts via CoinPayments are off-chain settled.

---

## 7. Architecture

### System diagram (text)

```
┌───────────────────────────────────────────────────────────────────┐
│                          CUSTOMERS                                 │
│  Browser <HLS> ───▶ Gateway Nodes <HTTP+libp2p>                    │
└───────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌───────────────────────────────────────────────────────────────────┐
│                     QUEST P2P NETWORK (Rust)                       │
│                                                                    │
│   ┌──────────┐   libp2p Kad-DHT    ┌──────────┐                    │
│   │  Node A  │◀──────────────────▶ │  Node B  │                    │
│   │  ─────   │                     │  ─────   │                    │
│   │ Storage  │                     │ Transcode│                    │
│   │ Worker   │                     │  Worker  │                    │
│   │ Gateway  │                     │ Gateway  │                    │
│   └──────────┘                     └──────────┘                    │
│        │                                 │                          │
│        └─────── erasure-coded chunks ────┘                          │
│                                                                    │
└───────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌───────────────────────────────────────────────────────────────────┐
│                  COORDINATOR PLANE (Bun + Supabase)                │
│                                                                    │
│  ┌─────────────┐  ┌──────────────┐  ┌──────────────────────┐       │
│  │  REST API   │  │ Job Dispatch │  │ Verification Engine  │       │
│  │   (Bun)     │  │    (Bun)     │  │       (Bun)          │       │
│  └─────────────┘  └──────────────┘  └──────────────────────┘       │
│                                                                    │
│  ┌────────────────────────────────────────────────────────────┐    │
│  │            Supabase (Postgres + Auth + Realtime)           │    │
│  │     videos, jobs, providers, earnings, challenges          │    │
│  └────────────────────────────────────────────────────────────┘    │
│                                                                    │
└───────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌───────────────────────────────────────────────────────────────────┐
│                    DASHBOARD (Next.js + Tailwind)                  │
│                       depin.quest/video                            │
└───────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌───────────────────────────────────────────────────────────────────┐
│                   CoinPayments (payouts/top-ups)                   │
└───────────────────────────────────────────────────────────────────┘
```

### Components

| Component | Language | Purpose |
|---|---|---|
| `quest` (node binary) | Rust | P2P daemon: storage, transcoding, gateway, CLI |
| Transcoding engine | FFmpeg (jellyfin-ffmpeg build) | Hardware-accelerated video encode/decode |
| Coordinator API | Bun (TypeScript) | REST/WebSocket API, job dispatch, billing, verification |
| Dashboard | Next.js 16 + Tailwind | Provider/customer/operator UI |
| Database | Supabase (Postgres) | Canonical state, auth, realtime |
| Payment rail | CoinPayments | Top-ups, payouts, ledger |

### URL namespace

All public surfaces live under `https://depin.quest/video`:

| Surface | Path |
|---|---|
| Marketing site | `/video` |
| Dashboard | `/video/app/...` |
| Embed iframe | `/video/embed/<videoId>` |
| Coordinator REST | `/video/api/v1/...` |
| CoinPayments IPN | `/video/api/v1/webhooks/coinpayments` |
| Install script | `/video/install.sh` |
| Release artifacts | `/video/releases/<version>/quest-<os>-<arch>.tar.gz` |
| Known-issues feed | `/video/api/v1/known-issues` |
| Public release manifest | `/video/api/v1/releases/latest` |

The Next.js app is configured with `basePath: "/video"` and the coordinator
API mounts at `/video/api/v1`.

---

## 8. Core Node — Rust Stack

### Crate selection

| Crate | Version target | Purpose |
|---|---|---|
| `libp2p` | 0.55+ | Networking, Kad-DHT, gossipsub, identify, ping |
| `tokio` | 1.40+ | Async runtime |
| `axum` | 0.8+ | HTTP server (gateway role) |
| `tower` / `tower-http` | latest | Middleware, tracing, compression |
| `serde` + `serde_json` | latest | Serialization |
| `bincode` | 2.x | Compact binary encoding for P2P messages |
| `blake3` | latest | Fast cryptographic hashing for content addressing |
| `reed-solomon-erasure` | latest | Erasure coding |
| `rocksdb` | latest | Local chunk store metadata |
| `sled` (alt) | — | Considered as alternative to RocksDB |
| `sqlx` | latest | Postgres client (for nodes that report direct to coordinator) |
| `reqwest` | latest | HTTP client (talking to coordinator API) |
| `clap` | 4.x | CLI argument parsing |
| `tracing` + `tracing-subscriber` | latest | Structured logging |
| `metrics` + `metrics-exporter-prometheus` | latest | Metrics |
| `self_update` | latest | Self-upgrade mechanism |
| `nix` (Unix) / `windows-rs` | latest | Platform integrations (service install) |
| `cudarc` | latest | Optional CUDA bindings (for direct GPU access where Mojo isn't used) |
| `ffmpeg-next` or `Command` invocation | — | FFmpeg integration |

### Why Rust

- Single static binary cross-compiled for Linux/macOS/Windows on x86_64 and aarch64
- libp2p has the best Rust implementation
- Memory safety + no GC = predictable performance for video workloads
- ~30MB binary is realistic; comparable Go binary would be 60–80MB
- Cross-compilation via `cross` is mature

### Roles a node can play (configurable)

A single node binary can take on any combination of these roles:

1. **Storage role** — accepts and stores erasure-coded chunks, serves on demand. Resource: disk + upload bandwidth.
2. **Transcode role** — accepts transcode jobs, runs FFmpeg, returns chunks. Resource: GPU/CPU.
3. **Gateway role** — public HTTP endpoint, fetches chunks from DHT, serves HLS to browsers. Resource: bandwidth + reasonable uptime.
4. **Verifier role** — runs random challenges against other nodes, reports results. Lightweight, all nodes do this opportunistically.

Configured via `quest start --roles storage,transcode` or via TOML config.

### Modules

```
quest/
├── crates/
│   ├── quest-cli/         # Binary entrypoint, CLI parsing
│   ├── quest-core/        # Node coordination, role dispatch
│   ├── quest-net/         # libp2p stack, DHT, chunk transport protocol
│   ├── quest-store/       # Local chunk storage, RocksDB, erasure coding
│   ├── quest-transcode/   # FFmpeg orchestration, job execution
│   ├── quest-gateway/     # HTTP gateway (axum), HLS serving
│   ├── quest-verify/      # Challenge/response, reputation scoring
│   ├── quest-update/      # Self-upgrade mechanism
│   ├── quest-doctor/      # Self-diagnostics & self-heal
│   ├── quest-proto/       # Shared protobuf/bincode types
│   └── quest-api/         # Coordinator API client
└── Cargo.toml
```

---

## 9. Transcoding Layer — FFmpeg

### Decision

FFmpeg is the transcoding engine end-to-end. No custom GPU code, no Mojo, no reinventing what already works. We ship the **jellyfin-ffmpeg** build, which has the strongest hardware acceleration patches and is the same build the `ffmpeg-over-ip` ecosystem standardized on.

### Why FFmpeg

- Mature hardware acceleration: NVENC, NVDEC, QSV, VAAPI, AMF, VideoToolbox — all the encoders that matter
- All the codecs we need: H.264, H.265/HEVC, AV1 (SVT-AV1, libaom, NVENC AV1, QSV AV1)
- Filtering pipeline (scene detection via `select`, scaling, denoising, deinterlacing) without writing a line of CUDA
- LGPL build for the minimum codec set; jellyfin-ffmpeg GPL build for the full set (including x264/x265)
- Battle-tested in production by every video company on earth

### Hardware acceleration matrix

| GPU vendor | Encoder | Decoder | Codecs (encode) | Notes |
|---|---|---|---|---|
| NVIDIA (Pascal+) | NVENC | NVDEC | H.264, HEVC | AV1 encode requires Ada (RTX 40-series) |
| NVIDIA (Ada+) | NVENC | NVDEC | H.264, HEVC, AV1 | Best AV1 perf/$ on consumer hardware |
| Intel (Gen 9+) | QSV | QSV | H.264, HEVC | AV1 encode on Arc / Gen 12.5+ |
| Intel Arc / Battlemage | QSV | QSV | H.264, HEVC, AV1 | Underrated — solid AV1 hardware encode |
| AMD (Vega+) | AMF | AMF | H.264, HEVC | AV1 encode on RDNA3 (RX 7000-series) |
| Apple Silicon | VideoToolbox | VideoToolbox | H.264, HEVC | macOS workers only |
| CPU fallback | x264, x265, SVT-AV1 | software | All | Default path when no GPU detected |

The node detects available encoders at startup via `ffmpeg -encoders` and `ffmpeg -hwaccels`, reports capabilities to the coordinator, and the dispatcher routes jobs to capable workers.

### How the transcode worker uses FFmpeg

The Rust worker (`quest-transcode` crate) shells out to `ffmpeg` rather than linking it via `ffmpeg-next`. Reasons:

1. License hygiene — subprocess boundary keeps GPL FFmpeg cleanly separated from our Apache-2.0 binary
2. Crash isolation — a segfaulting FFmpeg doesn't take down the worker
3. Easier upgrades — bump the FFmpeg binary independently of the node
4. Resource limits — we can `nice`/`ionice`/cgroup the FFmpeg process

The worker:
1. Pulls input chunk(s) from the network into a temp dir
2. Builds an FFmpeg command line based on the `TranscodeSpec` (codec, bitrate, resolution, hardware preference)
3. Spawns FFmpeg, parses progress from stderr, enforces a wall-clock budget
4. On success, hashes the output, splits into chunks, announces them on the DHT
5. Reports completion to the coordinator

### Example command lines

**1080p H.264 NVENC:**
```
ffmpeg -hwaccel cuda -hwaccel_output_format cuda \
  -i input.ts \
  -c:v h264_nvenc -preset p5 -tune hq -rc vbr -cq 23 \
  -b:v 5M -maxrate 7M -bufsize 10M \
  -c:a aac -b:a 128k \
  -f mpegts output.ts
```

**1080p AV1 NVENC (Ada+):**
```
ffmpeg -hwaccel cuda -hwaccel_output_format cuda \
  -i input.ts \
  -c:v av1_nvenc -preset p5 -tune hq -rc vbr -cq 28 \
  -b:v 3M -maxrate 4.5M \
  -c:a libopus -b:a 96k \
  -f mpegts output.ts
```

**CPU fallback AV1 (SVT-AV1):**
```
ffmpeg -i input.ts \
  -c:v libsvtav1 -preset 6 -crf 32 \
  -c:a libopus -b:a 96k \
  -f mpegts output.ts
```

### Quality verification

VMAF scoring for the verification layer also uses FFmpeg's `libvmaf` filter — no custom kernel, no Mojo, no reinvention:

```
ffmpeg -i original.ts -i transcoded.ts \
  -lavfi "[0:v][1:v]libvmaf=log_path=score.json:n_threads=4" \
  -f null -
```

The verifier worker parses `score.json` and compares against the threshold (default: VMAF ≥ 90 for "passed", below → flag for re-verification or slash).

### Scene detection for chunking

FFmpeg's built-in scene detection handles keyframe-aligned chunking:

```
ffmpeg -i input.mp4 \
  -vf "select='gt(scene,0.4)',showinfo" \
  -f null - 2>scene_log.txt
```

We parse `scene_log.txt` for scene-change timestamps, then segment on those (or every N seconds, whichever comes first). This matches what `av1an` does and gives us clean chunk boundaries without writing any custom analysis code.

### Phase 2 considerations (deferred, no commitment)

If we later want differentiated AI features (Real-ESRGAN-style upscaling, frame interpolation, ML-based encoder parameter tuning), those would warrant looking at GPU-native libraries. We'll evaluate when there's a paying customer asking for it. Until then: FFmpeg does everything we need.

---

## 10. Backend / Coordinator — Bun + Supabase

### Why Bun

- 3–4x faster cold start than Node
- Native TypeScript, no transpile step
- Built-in test runner, bundler, package manager
- Single runtime for HTTP server, WebSocket, scripts
- Compatible with most npm packages

### Why Supabase

- Postgres as canonical state of record
- Built-in auth (JWT, OAuth, magic link)
- Realtime subscriptions = live dashboard updates without bespoke WS plumbing
- Row-level security enforces multi-tenancy at DB level
- **Self-hostable via docker-compose** — meets sovereign customer (C4) requirement
- Open source (Apache 2.0 for most components)

### Services in the coordinator plane

```
coordinator/
├── apps/
│   ├── api/                # Bun HTTP server (axum-equivalent in TS: Hono)
│   │   ├── routes/
│   │   │   ├── videos.ts
│   │   │   ├── jobs.ts
│   │   │   ├── providers.ts
│   │   │   ├── earnings.ts
│   │   │   ├── billing.ts        # CoinPayments webhooks
│   │   │   └── webhooks.ts
│   │   └── server.ts
│   ├── dispatcher/          # Job dispatch worker
│   ├── verifier/            # Issues challenges, processes responses
│   ├── billing/             # Earnings calc, payout batching
│   └── jobs/                # Cron jobs (cleanup, metrics rollups)
├── packages/
│   ├── db/                  # Supabase client + types (generated)
│   ├── proto/               # Shared types with Rust nodes
│   └── coinpayments/        # CoinPayments SDK wrapper
└── package.json (Bun)
```

### Key API endpoints

All paths are mounted under `/video/api/v1`:

```
POST   /video/api/v1/videos                    Create video record, get upload URL
POST   /video/api/v1/videos/:id/finalize       Mark upload complete, queue transcode
GET    /video/api/v1/videos/:id                Get video status & manifest URL
DELETE /video/api/v1/videos/:id                Soft delete

POST   /video/api/v1/providers/register        Provider node registration
POST   /video/api/v1/providers/:id/heartbeat   Liveness + capacity report
GET    /video/api/v1/providers/:id/earnings    Earnings history

POST   /video/api/v1/jobs/claim                Worker claims next available job
POST   /video/api/v1/jobs/:id/complete         Worker submits result
POST   /video/api/v1/jobs/:id/fail             Worker reports failure

POST   /video/api/v1/challenges/:id/respond    Provider responds to verification challenge

POST   /video/api/v1/billing/topup             Customer top-up (CoinPayments)
POST   /video/api/v1/billing/withdraw          Provider withdrawal request
POST   /video/api/v1/webhooks/coinpayments     CoinPayments IPN endpoint

GET    /video/api/v1/network/health            Public network stats (peer count, etc)
GET    /video/api/v1/releases/latest           Current release manifest for self-upgrade
GET    /video/api/v1/known-issues              Known-issues feed for `quest doctor`
```

### Realtime subscriptions (Supabase)

- `earnings:provider_id=X` — live earnings updates for a provider
- `jobs:status=running` — operator dashboard sees jobs in flight
- `videos:owner_id=X` — customer dashboard sees their video status update

---

## 11. Frontend / Dashboard — Next.js 16 + Tailwind

The Next.js app uses `basePath: "/video"`. All routes below are relative to
that basePath.

### Routes

```
/                           Marketing site            (→ /video)
/docs                       Developer docs            (→ /video/docs)
/install                    CLI install instructions  (→ /video/install)
/install.sh                 The install script        (→ /video/install.sh, served as text/plain)

/auth/login                 Magic link login
/auth/callback              Auth callback

/app                        Authenticated shell

  /app/customer
    /videos                 Video library with status
    /videos/:id             Video detail (analytics, embed, player preview)
    /upload                 Upload new video
    /billing                Top-ups, usage breakdown, invoices
    /api-keys               API key management

  /app/provider
    /overview               Earnings, utilization, provider score
    /nodes                  Node list, status, hardware
    /nodes/:id              Node detail (jobs, earnings, logs)
    /payouts                Payout history, withdrawal
    /settings               Node settings (roles, capacity caps)

  /app/operator (admin only)
    /network                Network health, peer map
    /jobs                   All jobs, fraud detection
    /providers              Provider management
    /finance                Revenue, margins, payouts

/embed/:videoId             Embeddable HLS player iframe
```

### Component approach

- shadcn/ui as base components
- Tailwind for everything (no CSS modules, no styled-components)
- Recharts for graphs
- HLS.js for the embedded player
- React Server Components by default; client components only where needed (forms, charts, realtime)
- Supabase realtime hooks for live dashboards

### Player

The embeddable player at `/embed/:videoId` is a custom HLS.js wrapper with a custom loader that fetches segments through a Quest gateway. Falls back to native HLS on Safari. ~200 lines of TypeScript.

```tsx
// player loader sketch
class QuestLoader extends Hls.DefaultConfig.loader {
  load(context, config, callbacks) {
    if (context.url.startsWith('quest://')) {
      const hash = context.url.replace('quest://blake3:', '')
      context.url = `${getGateway()}/chunks/${hash}`
    }
    super.load(context, config, callbacks)
  }
}
```

---

## 12. CLI Design

### Install

Users hit `depin.quest/video/install` and see:

```bash
curl -fsSL https://depin.quest/video/install.sh | sh
```

The install script:

1. Detects OS (Linux/macOS/Windows-via-WSL) and arch (x86_64/aarch64).
2. Downloads the latest release from `https://depin.quest/video/releases/latest/depin-<os>-<arch>.tar.gz`.
3. Verifies SHA-256 against the published checksum.
4. Verifies the minisign signature using a hardcoded public key (rotation policy in §13).
5. Installs to `~/.depin/bin/depin` (no sudo required).
6. Adds `~/.depin/bin` to PATH via shell profile.
7. Runs `depin video doctor` to verify the install.
8. Prints next-step instructions.

### Commands

The binary is `depin`. Commands are nested under product-line subcommands so
future lines (`depin storage`, `depin compute`) can ship in the same binary
without name collisions. Today the only line is `depin video`.

```
depin video start [--roles storage,transcode,gateway] [--storage 500GB] [--gpu]
depin video stop
depin video status
depin video restart
depin video config set <key> <value>
depin video config get <key>
depin video config list
depin video doctor                         # Run diagnostics, suggest/apply fixes
depin video doctor --fix                   # Apply fixes automatically
depin video doctor --report                # Upload anonymized diagnostics to dashboard
depin video upgrade                        # Manual upgrade (auto-upgrade is default)
depin video upgrade --check                # Check for new version, don't install
depin video logs [--follow] [--since 1h]
depin video peers                          # List connected peers
depin video jobs                           # Show recent jobs and earnings
depin video earnings                       # Earnings summary
depin video withdraw <amount> <currency>   # Trigger payout via CoinPayments

# Customer-facing commands (for upload-from-CLI users)
depin video upload <file> [--rendition 1080p,720p,480p]
depin video videos                         # List uploaded videos
depin video show <id>                      # Inspect a single video

# Top-level
depin version                              # Print binary version
depin --help                               # List product lines
```

### Self-upgrade mechanism

- On startup and every 6 hours, the node queries `https://depin.quest/video/api/v1/releases/latest`.
- If a newer version exists with a higher minimum-required version than the current binary, the node:
  1. Downloads the new binary to a temp path.
  2. Verifies SHA-256 + minisign signature.
  3. Atomically swaps the binary using `self_update` crate semantics.
  4. Re-execs itself with the same arguments and PID inheritance where possible.
- Configurable: `quest config set update.channel stable|beta|nightly`
- Configurable: `quest config set update.auto false` (still upgrades for security advisories)
- A blocked rollback list prevents downgrade past known-vulnerable versions.

### Self-error-fixing (`quest doctor`)

The `doctor` command runs through a checklist and either reports or auto-fixes common issues:

| Check | Fix |
|---|---|
| FFmpeg present + version compatible | Download bundled jellyfin-ffmpeg into `~/.depin/bin/ffmpeg` |
| GPU drivers detected | Detect missing CUDA/NVENC, link to vendor docs (don't auto-install drivers — security risk) |
| Disk space ≥ configured cap + 10% headroom | Warn; offer to reduce storage cap |
| Open inbound ports (configured QUIC port) | Test via STUN-like check; suggest UPnP toggle or hole-punch fallback |
| Clock drift ≤ 30s | Run `chronyd`/`w32tm` resync if root, else warn |
| RocksDB store integrity | Run integrity check, attempt repair with backup |
| Stuck jobs (>1h running) | Mark failed, requeue |
| DHT routing table healthy (>50 peers) | Force re-bootstrap |
| Outbound connectivity to coordinator | Test, suggest proxy config |
| Recent error log clusters | Match against known-issues feed at `depin.quest/video/api/v1/known-issues`; apply server-suggested fix |

The "known-issues feed" is the killer feature: when a bug ships, we publish a remediation entry that says "if you see error pattern `EOF in transcode worker`, set `transcode.timeout = 600`". The node fetches this feed periodically and `quest doctor` references it. This means we can fix entire fleets of bad behavior without a binary release.

### Self-heal daemon

While running, the node has a watchdog that:
- Restarts crashed worker subprocesses with exponential backoff (max 5 attempts/15 min)
- Reconnects to coordinator/peers on network failure
- Drops in-flight jobs that exceed their wall-clock budget
- Throttles or disables roles when local resource pressure exceeds thresholds (e.g., disk >95% full → stop accepting new chunks)
- Submits anonymized crash dumps to coordinator (opt-in; default on with PII scrubbing)

---

## 13. Data Model

### Core entities (Postgres / Supabase)

See `supabase/migrations/0001_init.sql` for the canonical schema. High-level
tables:

- `profiles` — extends `auth.users` with role + payout settings
- `videos` — customer-owned video records
- `renditions` — derived encodes per video
- `chunks` + `shard_sets` — content-addressed pieces with erasure coding metadata
- `providers` — registered nodes (hardware, capabilities, reputation, stake)
- `jobs` — transcode units claimed by workers
- `challenges` — verification challenges + responses
- `earnings` — append-only provider ledger
- `billing` — append-only customer ledger (top-ups + charges)

### Row-level security

Customers see only their own videos/billing. Providers see only their own nodes/earnings. Operators see everything. Enforced at Postgres via Supabase RLS policies, not at app layer.

### Network protocol types (Rust + Bun, generated from `quest-proto`)

```rust
struct ChunkAnnouncement {
    chunk_hash: [u8; 32],
    shard_index: u8,
    bytes: u32,
    expires_at: u64,
}

struct ChunkRequest {
    chunk_hash: [u8; 32],
    shard_index: Option<u8>,
}

struct TranscodeJob {
    job_id: Uuid,
    input_chunk_hash: [u8; 32],
    spec: TranscodeSpec,
    output_target: OutputTarget,
    deadline: u64,
}

struct TranscodeSpec {
    codec: Codec,             // H264, HEVC, AV1
    bitrate_bps: u32,
    width: u32,
    height: u32,
    keyframe_interval: u32,
    extra_ffmpeg_args: Vec<String>,
}
```

---

## 14. Network Protocol

### Discovery

- **Bootstrap:** Hardcoded list of bootstrap multiaddrs in the binary (operator-run reliable nodes). Configurable via `--bootstrap`.
- **DHT:** libp2p Kad-DHT with custom protocol identifier `/quest/kad/1.0.0` to keep our network separate from the public IPFS network.
- **mDNS:** For LAN discovery (useful in homelab/datacenter scenarios).
- **Identify protocol:** Standard libp2p, advertises capabilities (`quest:transcode-h264-nvenc`, `quest:storage-tier-hot`, etc).

### Chunk transport

- **Transport:** QUIC (libp2p-quic). Falls back to TCP+Noise if QUIC blocked.
- **Streaming:** Chunks are fetched in 64KB frames over a single QUIC stream per chunk.
- **Parallel fetch:** Gateway races top-3 providers per chunk; first to deliver wins, others cancel.
- **Verification:** Every fetched chunk is hashed (blake3); mismatch → ban that provider for the chunk and retry.

### Job dispatch

- Worker connects to coordinator API, sends heartbeat with capabilities + free capacity.
- Coordinator's dispatcher selects job that matches (codec support, hardware tier, region preference).
- Worker claims job via `POST /video/api/v1/jobs/claim` with optimistic concurrency (job goes from `queued` → `running` atomically).
- Worker fetches input chunk(s) from network, runs transcode, announces output chunks to DHT, reports completion to coordinator.
- Coordinator verifies output via random chunk re-hash + occasional full re-transcode of a sample.

### Verification

Two layers:

**1. Storage verification (Proof of Replication-lite).** Every provider stores chunk shards. Coordinator picks random chunks from random providers periodically and challenges:

```
challenge: hash(chunk_bytes[offset..offset+1024])
provider must respond within 30s
```

Failures decrement reputation. Three failures within 24h → suspended for 24h. Suspensions accumulate.

**2. Transcode verification (Proof of Useful Work-lite).** A small percentage (~2%) of completed jobs are re-run on a different worker. Outputs compared via VMAF score and chunk hash. Mismatch beyond tolerance → original worker slashed (forfeits payout for that job + reputation hit).

We don't do zero-knowledge proofs of computation in v1. We rely on economic stake + reputation + spot-check verification, which is the same model as Livepeer and is sufficient.

### Reputation scoring

```
reputation = 0.5 + (0.3 * uptime_30d) + (0.15 * verification_pass_rate) + (0.05 * job_completion_rate) - (0.5 * recent_slash_weight)
```

Bounded [0, 1]. Job dispatch weights provider selection by reputation × capability match.

---

## 15. Economic Model

### Pricing (target launch numbers)

| Service | Price to customer | Provider receives | Network keeps |
|---|---|---|---|
| Transcode (H.264, 1080p) | $0.005 / output minute | $0.0040 | $0.0010 |
| Transcode (AV1, 1080p) | $0.012 / output minute | $0.0095 | $0.0025 |
| Storage | $0.005 / GB-month | $0.0040 | $0.0010 |
| Egress | $0.003 / GB | $0.0024 | $0.0006 |
| Gateway request | $0.0001 / request | $0.00008 | $0.00002 |

Network take rate: ~20%.

For comparison: Mux charges $0.04/min for 1080p H.264 encoding. Bunny.net charges $0.01/GB for storage and ~$0.005/GB for egress in cheap regions. We're 50–87% cheaper.

### Payouts

- Earnings accrue per job in the `earnings` table.
- Daily batch settlement: at 00:00 UTC, sum each provider's pending earnings.
- Above $10 threshold → trigger CoinPayments transfer to provider's configured wallet.
- Below $10 → roll forward to next day.
- Provider can withdraw on-demand for a $0.50 flat fee via `quest withdraw`.

### Top-ups

- Customer initiates top-up in dashboard for any USD amount.
- Dashboard creates CoinPayments invoice (USDC/USDT/BTC/ETH/etc).
- Customer pays. CoinPayments IPN webhook hits `/video/api/v1/webhooks/coinpayments`.
- We credit `billing` ledger.
- Customer's available credit = sum(topups) - sum(charges).
- Auto-charge model possible later (saved card via CoinPayments or direct on-chain authorization).

### Stake & slash

- Providers can optionally stake (denominated in USD-pegged stablecoin via CoinPayments hold).
- Higher stake → eligible for higher-tier jobs (better paying, larger customers).
- Slashable offenses:
  - Verification challenge failure: -2% of stake per failure
  - Transcode fraud (fake output): -25% per incident
  - Sybil detection: full stake
- v1 implementation: stake held in our CoinPayments custody account, slashes go to insurance pool that pays out to affected customers.

---

## 16. Security

### Threat model

| Threat | Mitigation |
|---|---|
| Malicious provider serves wrong chunks | Content addressing — every chunk verified by hash on receipt |
| Malicious provider claims to store chunks but doesn't | Periodic Proof-of-Replication challenges with random offsets |
| Malicious provider returns garbage transcode | VMAF-based verification on sample; slash on mismatch |
| Sybil attack (one entity many nodes to game payouts) | Stake requirement + KYC at withdrawal threshold + IP/ASN diversity scoring |
| DHT eclipse attack | S/Kademlia node ID generation (proof-of-work for peer ID) |
| Coordinator compromise | Coordinator state in Supabase; provider earnings double-entry; cryptographic receipts customers can verify |
| MITM between gateway and viewer | TLS to gateway; chunk integrity verified by hash even if TLS fails |
| Customer uploads CSAM/illegal content | DMCA agent + abuse@depin.quest + content hash blocklist; legally we're a service provider per DMCA 512 |
| Providers serving illegal content unwillingly | Deny-by-hash list distributed to all gateways and storage nodes |
| Compromised coordinator updates malicious binary | All releases minisigned; key rotation requires multi-sig from project maintainers |

### Encryption

- All P2P traffic: Noise XX (libp2p default) — equivalent to TLS 1.3.
- Customer can opt for E2E-encrypted storage: chunks encrypted with customer-held key before being announced to network. Network never sees plaintext.
- For E2E mode, transcoding cannot happen on-network (workers can't read plaintext) — customer transcodes locally, network just stores. This is the C4 sovereign-content mode.

### Key management

- Node identity: ed25519 keypair generated on first run, stored in `~/.depin/identity.key` (chmod 600).
- Customer API: Supabase JWT.
- CoinPayments: HMAC-SHA512 on all API calls per their spec.

### Release signing

- Binaries signed with minisign.
- Public key embedded in install script and current binary.
- Key rotation: new key signed by N-1 old keys (3-of-5 multisig at project maintainer level).
- Self-upgrade refuses to install binaries signed by revoked keys.

---

## 17. Performance Targets

### Latency

- P50 first-frame for cached video: ≤1.2s
- P95 first-frame for cached video: ≤2.5s
- P95 first-frame for cold video (no cached gateway, fresh DHT lookup): ≤6s
- P99 chunk fetch (cached): ≤300ms
- P99 chunk fetch (cold): ≤1.5s

### Throughput

- Single transcode worker (RTX 4070): ≥4 concurrent 1080p H.264 streams in real-time
- Single gateway: ≥2 Gbps egress sustained
- Network as a whole: scales linearly with peer count

### Reliability

- 99.9% chunk availability for content with default 10/14 erasure coding
- 99.95% gateway availability (dashboard SLO)
- Coordinator API 99.9% (Supabase SLO)

### Cost (cloud comparison)

- Transcode 1 hour of 1080p H.264 source → 4 renditions: ≤ $0.30 (Mux: $1.60; AWS MediaConvert: $0.90)
- Store 1 TB for 1 month: ≤ $5 (S3: $23; B2: $6)
- Egress 1 TB: ≤ $3 (CloudFront: $85; Bunny: $5)

---

## 18. FOSS Licensing

### License selection per component

| Component | License |
|---|---|
| `quest` (Rust node) | Apache-2.0 |
| `quest-proto` | Apache-2.0 |
| Coordinator API (Bun) | Apache-2.0 |
| Dashboard (Next.js) | Apache-2.0 |
| Install scripts | Apache-2.0 |
| Documentation | CC-BY-4.0 |
| FFmpeg dependency | LGPL-2.1+ (we link, don't redistribute modified) — bundled jellyfin-ffmpeg builds carry GPL-3 due to specific patches; we ship those as a separate optional download |

### CLA / DCO

- DCO sign-off (Linux-kernel-style) on all commits. No CLA. Lower friction for contributors.

### Trademark

- "Quest" and "depin.quest" are trademarks; usage policy documented at `depin.quest/video/trademark`. Code is libre; the brand is not.

---

## 19. Roadmap

### Milestone 0 — Foundation (weeks 1–4)

- Rust workspace skeleton, libp2p + Kad-DHT bootstrap
- Local chunk store with blake3 content addressing
- Two-node demo: announce + fetch chunk over LAN
- CoinPayments sandbox account integrated
- Supabase schema migrations

### Milestone 1 — Single-rendition VOD (weeks 5–10)

- HLS segmenter (FFmpeg-driven keyframe-aligned chunking)
- Gateway node serves HLS from local + DHT-fetched chunks
- HLS.js player on dashboard plays a video
- Coordinator API + dispatcher MVP
- Single worker transcodes a job end-to-end
- Customer uploads → transcoded → playable

**Demo deliverable:** Upload a 2-minute MP4 via dashboard, get a Quest URL, embed on a test page, video plays.

### Milestone 2 — Multi-rendition + erasure coding (weeks 11–16)

- Reed-Solomon erasure coding (k=10, n=14)
- Multi-rendition transcoding (1080p/720p/480p × H.264)
- Replication policy: 3 random + opportunistic caching
- Verification challenges (storage)
- 10-node testnet running in CI

### Milestone 3 — Economy & dashboard (weeks 17–22)

- Earnings ledger
- CoinPayments live integration: top-ups + payouts
- Provider dashboard (earnings, jobs, status)
- Customer dashboard (videos, billing, embed)
- Operator dashboard (network health)
- Reputation scoring

### Milestone 4 — Public beta (weeks 23–28)

- AV1 transcoding
- Self-upgrading CLI with `curl|sh` install
- `quest doctor` with auto-fix
- Public bootstrap nodes
- 100-node testnet
- First 10 paying customers

### Milestone 5 — Scale & Phase-2 features (weeks 29+)

- AV1 encode optimization (rate control tuning, two-pass profiles)
- AI upscaling premium feature (evaluate Real-ESRGAN via ONNX runtime if customer demand exists)
- Multi-region gateway placement
- Live streaming support (separate PRD)
- DRM exploration (Widevine partnership)

---

## 20. Success Metrics

### North-star metric

- **GMV processed through the network per month** (transcode + storage + egress).

### Phase 1 success criteria (end of Milestone 4)

- ≥100 active provider nodes
- ≥10 paying customers
- ≥$5K MRR
- Working self-upgrade in production
- ≥99% chunk availability for live content

### Phase 2 success criteria (end of Milestone 5)

- ≥1000 active providers
- ≥100 paying customers
- ≥$100K MRR
- ≤30% of revenue going to refunds/disputes
- Net negative provider churn (we add more than we lose)

---

## 21. Risks & Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Network can't reach price parity with cloud | Medium | High | Don't compete on raw price for cold storage; lead with transcode pricing where margins are best |
| Provider supply doesn't bootstrap | High | High | Operator-run anchor nodes (us) handle 100% of demand at launch; subsidize early provider earnings via 0% take rate for first 1000 jobs/provider |
| Money-transmitter regulation in US | Medium | High | CoinPayments is the regulated rail; we structure as B2B marketplace not money transmitter; pre-launch legal review with crypto-fintech counsel |
| CSAM / illegal content liability | Medium | Critical | DMCA 512 safe harbor compliance; hash blocklist; mandatory abuse reporting; T&C bans illegal content |
| Coordinator becomes single point of failure | High | High | Federation roadmap (Milestone 6); customers can self-host coordinator; multi-region active-active |
| FFmpeg licensing complications (GPL bundle) | Low | Medium | Two install paths: minimal (LGPL FFmpeg, fewer codecs) and full (jellyfin-ffmpeg, GPL); user opts in |
| libp2p DHT performance under load | Medium | Medium | Custom protocol ID isolates us from public IPFS; private bootstrap; benchmark at 1K, 10K, 100K nodes early |
| CoinPayments outage / depeg risk | Medium | High | Settle daily not real-time; abstract payment provider behind interface; have BTCPay/secondary integration on roadmap |
| Self-upgrade ships a bad binary | Low | Critical | Staged rollout (1% → 10% → 100% over 48h); emergency rollback flag in release feed; signature verification; integration tests gate releases |

---

## 22. Open Questions

1. **Pricing model:** flat per-unit or auction/spot pricing?
2. **Stake required at launch?** Or just for premium tier?
3. **Coordinator federation:** when do we build it, and is the architecture decentralized-state or just multi-region active-active?
4. **DRM:** is partnership with Widevine/PlayReady worth the centralization, given the premium-content money it unlocks?
5. **Token decision:** firm "no token" stance, or revisit if we need to bootstrap supply faster?
6. **Brand:** is "Quest" the product name or just the dashboard? Lock the naming before public launch.
7. **Network effect strategy:** do we incentivize creator-bring-creator, or focus on enterprise sales?

---

## 23. Out of Scope (explicitly)

- Mobile apps (web mobile is fine)
- Native desktop player (web embed sufficient)
- Live streaming (Phase 3+)
- Audio-only / podcast hosting
- File hosting beyond video
- Social features, comments, reactions
- Recommendation engine
- Token / governance / DAO
- Built-in analytics beyond per-video views
- Translation, captioning, accessibility tooling (defer to customer)

---

## Appendix A — Sample install.sh

The actual script lives at `scripts/install.sh` and is served from
`https://depin.quest/video/install.sh`. Sketch:

```bash
#!/usr/bin/env sh
set -e

QUEST_VERSION="${QUEST_VERSION:-latest}"
QUEST_HOME="${QUEST_HOME:-$HOME/.quest}"
RELEASE_BASE="https://depin.quest/video/releases"

detect_platform() {
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)
  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) echo "Unsupported arch: $arch" >&2; exit 1 ;;
  esac
  case "$os" in
    linux|darwin) ;;
    *) echo "Unsupported OS: $os" >&2; exit 1 ;;
  esac
  echo "${os}-${arch}"
}

main() {
  platform=$(detect_platform)
  url="${RELEASE_BASE}/${QUEST_VERSION}/quest-${platform}.tar.gz"
  sig_url="${url}.minisig"

  mkdir -p "$QUEST_HOME/bin"
  echo "→ Downloading Quest ${QUEST_VERSION} for ${platform}..."
  curl -fsSL "$url" -o /tmp/quest.tar.gz
  curl -fsSL "$sig_url" -o /tmp/quest.tar.gz.minisig

  echo "→ Verifying signature..."
  # Embedded minisign pubkey (rotation handled per §16)
  # ... verification logic ...

  echo "→ Installing to $QUEST_HOME/bin..."
  tar -xzf /tmp/quest.tar.gz -C "$QUEST_HOME/bin"
  chmod +x "$QUEST_HOME/bin/quest"

  for rc in ~/.bashrc ~/.zshrc ~/.profile; do
    if [ -f "$rc" ] && ! grep -q "QUEST_HOME" "$rc"; then
      echo "export PATH=\"\$HOME/.quest/bin:\$PATH\"" >> "$rc"
    fi
  done

  echo "→ Running diagnostics..."
  "$QUEST_HOME/bin/quest" doctor || true

  cat <<EOF

✓ Quest installed to $QUEST_HOME/bin/quest

Next steps:
  1. Restart your shell or:  export PATH="\$HOME/.quest/bin:\$PATH"
  2. Register your node:      quest config set api.token <YOUR_TOKEN>
                              (get one at https://depin.quest/video/app/provider)
  3. Start earning:           quest start --roles storage,transcode

Docs: https://depin.quest/video/docs

EOF
}

main "$@"
```

---

## Appendix B — Reference architecture decisions

Decisions made and why, for future reviewers.

- **Rust over Go for the node.** libp2p-rust is the most actively developed implementation; memory predictability matters for video buffers; binary size advantage.
- **libp2p over Hypercore.** Multi-tenant model fits our use case better; bigger ecosystem; multiple language bindings (we'll need them when partners want SDKs).
- **blake3 over SHA-256.** ~5x faster, same security, content-addressing throughput matters at scale.
- **Reed-Solomon over plain replication.** Same durability at lower storage overhead.
- **FFmpeg via subprocess over linked library.** License hygiene, crash isolation, easier upgrades, easier resource limits. Negligible performance cost for our chunk sizes.
- **jellyfin-ffmpeg over vanilla FFmpeg.** Best hardware acceleration patches; same build the broader homelab/Plex/Jellyfin/Emby ecosystem standardized on.
- **Bun over Node for backend.** Faster cold start, better TypeScript ergonomics, single tool for runtime/test/bundle.
- **Supabase over rolling our own Postgres.** Auth, RLS, realtime are commodity now; self-hostable so we keep the FOSS story.
- **CoinPayments over building our own payments.** Money transmission is a regulatory swamp; we don't want to be the regulated entity in v1.
- **HLS over DASH.** Wider compatibility, especially Safari/iOS. DASH addable later.
- **No token in v1.** Avoids regulatory minefield; lets us focus on product-market fit; can always add later.
- **All surfaces under `/video` namespace.** `depin.quest` is reserved as the parent brand; every dashboard route, API path, install script, and release artifact mounts under `/video` so future product lines (e.g. `/storage`, `/compute`) can coexist on the same domain without collision.

---

*End of PRD v0.1*
