# Quest

> Decentralized video transcoding & hosting. Every public surface lives under
> `https://depin.quest/video/...`.

Source of truth for product, architecture, and roadmap: [`docs/PRD.md`](docs/PRD.md).

---

## Repo layout

```
p2p-one/
├── docs/
│   └── PRD.md                 # full product requirements doc (v0.1)
├── node/                      # Rust workspace — the `quest` binary + crates
│   └── crates/
│       ├── quest-cli/         # binary entrypoint
│       ├── quest-core/        # config + supervisor
│       ├── quest-net/         # libp2p layer (scaffold)
│       ├── quest-store/       # content-addressed chunk store
│       ├── quest-transcode/   # FFmpeg orchestration
│       ├── quest-gateway/     # axum HTTP gateway role
│       ├── quest-verify/      # challenges + reputation
│       ├── quest-update/      # self-upgrade
│       ├── quest-doctor/      # self-diagnostics
│       ├── quest-proto/       # shared types
│       └── quest-api/         # coordinator HTTP client
├── apps/
│   ├── web/                   # Next.js 16 dashboard, basePath /video
│   ├── coordinator/           # Bun + Hono REST API at /video/api/v1
│   └── cli/                   # legacy TypeScript CLI (kept for now)
├── packages/
│   └── shared/                # shared TS types (mirrors quest-proto)
├── supabase/
│   └── migrations/0001_init.sql
└── scripts/
    └── install.sh             # served at /video/install.sh
```

## Quickstart

### Run the Rust node locally

```bash
bun run node:test                       # all crate tests
bun run node:run -- doctor              # diagnostic checklist
bun run node:run -- --help
```

### Run the coordinator API

```bash
cp apps/coordinator/.env.example apps/coordinator/.env
# fill in SUPABASE_URL + SUPABASE_SERVICE_ROLE_KEY
bun run dev:coordinator
# → http://localhost:8787/video/api/v1/...
```

### Run the dashboard

```bash
cp apps/web/.env.local.example apps/web/.env.local
bun run dev:web
# → http://localhost:3000/video
```

### Apply the Supabase schema

Point your Supabase CLI at the project, then:

```bash
supabase db push
# or apply 0001_init.sql directly via the SQL editor
```

## Status

This is the scaffold for Milestone 0 of the PRD. Working today:

- Rust workspace compiles and 12 unit tests pass (`cargo test`)
- `quest doctor` runs a real checklist
- Coordinator boots, serves `/video/health` and route stubs
- Dashboard rebrands to Quest under `/video` basePath
- Supabase schema covers all PRD §13 tables + RLS policies + atomic
  `claim_next_job()` RPC

Not yet wired up (see PRD roadmap):

- libp2p stack — `quest-net` is a trait surface today
- CoinPayments live integration — sandbox creds outstanding
- Auth flow on the dashboard
- HLS player on `/embed/:videoId`

## License

Apache-2.0 across all our code. See PRD §18 for the FFmpeg licensing dance.
