---
dip: 0011
title: "No central backend; libp2p + CoinPay are the source of truth"
status: Accepted
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-05-03
discussion:
implementation: deleted apps/coordinator and supabase/ in this commit
supersedes:
superseded-by:
---

## Summary

c0mpute has **no central application backend**. Discovery, dispatch,
verification, reputation, and payments all flow through:

- **libp2p Kad-DHT + gossipsub** for peer / capability / job discovery
  and announcements.
- **CoinPay DID + escrow** for identity, payments, signed receipts,
  and reputation aggregation (CoinPay is its own product, not a
  c0mpute subservice).

The only public infrastructure we host is **static**:

- The c0mpute landing site at `c0mpute.com` (apex Next.js app).
- Release tarballs at `c0mpute.com/releases/...`.
- The bootstrap-seed JSON at `c0mpute.com/bootstrap.json` (DIP-0010).
- Hosting for module manifests at `c0mpute.com/modules/...`.

No Postgres. No Supabase. No Hono / Express coordinator daemon. No
session cookies. No user table on a server we run.

## Motivation

The original c0mpute v1 PRD §"MVP API Concepts" listed a Bun + Hono
coordinator with a Postgres `providers` / `jobs` / `earnings` schema.
Every additional moment of looking at it surfaced the same question:
**why?** A "decentralized compute network" with a centralized
coordinator that knows about every job, every worker, and every
payment is just a marketplace with extra steps.

Three forcing facts:

1. **Auth is CLI-driven** (DIP-0008). `coinpay did create` and
   `c0mpute worker register` are CLI commands. There's no web login
   that needs sessions, no dashboard that reads user-specific data
   over an authenticated REST API.
2. **Payments are CoinPay's job** (DIP-0007). CoinPay handles escrow,
   receipts, reputation. We don't need to reinvent that table in our
   own DB.
3. **Discovery is libp2p's job** (DIP-0010 + PRD §14). Kad-DHT +
   gossipsub do this without a central registry.

What's left for a "coordinator" to do that libp2p + CoinPay don't?
Not enough to justify the complexity, the SPOF, the censorship vector,
or the privacy footprint.

## Detailed design

### What each function looks like without a coordinator

| Function | How it works in this design |
|---|---|
| Worker discovery | libp2p Kad-DHT advertisements with capability tags (`c0mpute:transcode`, `c0mpute:gpu:nvidia`, `c0mpute:storage:hot`) |
| Job dispatch | Buyer signs job manifest with their DID → posts to a gossipsub topic (`c0mpute/jobs/<workload-type>`) → eligible workers race to claim with signed proof; CoinPay arbitrates conflicts via escrow timing |
| Job manifest storage | Content-addressed (blake3); the manifest IS its hash. Buyers reference jobs by hash |
| Provider heartbeats | gossipsub keepalive on `c0mpute/heartbeat/<peer-id>` topic with TTL |
| Reputation | Signed receipts → CoinPay aggregates per DID; clients query CoinPay or any aggregator node |
| Verification challenges | Validators self-pick from gossipsub; results signed and posted back to the same topic; CoinPay weights them |
| Billing / escrow | CoinPay (decentralized via DID + escrow primitives); `c0mpute coinpay escrow create` is a CLI command, not a coordinator API call |

### What we still host as static infra

- `c0mpute.com/` — marketing landing (apps/web in this repo)
- `c0mpute.com/releases/<version>/<artifact>` — binary tarballs +
  minisigs
- `c0mpute.com/bootstrap.json` — bootstrap seed list (DIP-0010)
- `c0mpute.com/modules/<id>/...` — plugin marketplace metadata (per
  DIP-0006: prefetch URLs, manifest mirrors). All static / cacheable.
- `c0mpute.com/known-issues.json` — feed consumed by `c0mpute doctor`
  for fleet-wide remediation hints (per PRD §12)

These are all *files* served by a CDN / static host. There's no
application state. If the host goes down, the network keeps running
(existing peers still talk to each other; only new peer onboarding +
self-update is degraded until the static host comes back).

### What about the things we lose?

| Lost | Replacement |
|---|---|
| Supabase Auth | Not needed — CLI-only auth via `coinpay did create` |
| Realtime subscriptions for the dashboard | The dashboard is a static landing for v1 (DIP-0008); future per-plugin dashboards can poll CoinPay APIs or subscribe to gossipsub via a light-client lib |
| RLS for multi-tenancy | Trivially: clients only see jobs / receipts they have keys for, signed under their DID |
| `claim_next_job()` SQL atomicity | gossipsub fan-out + signed proof of claim + CoinPay escrow timing arbitration. Slightly more design work, but doesn't require shared mutable state |
| Centralized abuse / DMCA intake | Profullstack still maintains an abuse@ email + content-hash blocklist served as a static file; gateways and storage workers refuse blocked hashes. Same legal posture without a server-side DB |

### Marketplace plugin store

Per DIP-0006 the plugin store at `c0mpute.com/plugins` (web view +
JSON API at `c0mpute.com/api/plugins/<id>`) is *static* in v1:

- Plugin metadata is committed to this repo under `plugins/<id>/module.toml`.
- The Next.js app reads those at build time and renders the store page.
- Install commands the page shows just chain to upstream `install.sh` URLs.

When we want third-party plugin submissions, that *might* require a
small backend for receiving submissions, but the store itself stays
static (submissions become PRs to this repo, or to a separate manifest
repo with a moderation queue).

## Alternatives considered

**Keep the coordinator for "v1 simplicity".** The coordinator was
indeed simpler to build first. But it was costing us more in design
energy ("how does this not become a central trust assumption?") than
it was saving.

**Hybrid: coordinator for low-stakes things (heartbeat, dashboard
data); p2p for high-stakes things (jobs, receipts).** Tempting, but
in practice the coordinator just becomes a shim that everyone routes
around once the p2p layer exists. Drop it now.

**Keep Supabase but only as the abuse / DMCA / public-stats backend.**
Still a server-side DB to maintain. Static files + community-run
aggregators handle public stats; abuse intake is an email address +
a static blocklist file. No DB needed.

## Migration & rollout

Performed in this commit:

- Deleted `apps/coordinator/` (Bun + Hono REST API, all routes,
  Supabase client wrapper, CoinPayments wrapper).
- Deleted `supabase/` (migration with profiles / videos / providers /
  jobs / earnings / billing tables + RLS policies).
- Updated `package.json` workspace scripts to drop coordinator-related
  entries.
- Updated `README.md` to reflect the no-backend posture.

Things that move forward:

- Job manifest schema lives in code (`c0mpute-proto` Rust crate + the v1
  PRD §"Job Manifest Format"); doesn't need a DB.
- The `claim_next_job()` SQL RPC was a useful documentation of intent;
  the equivalent gossipsub-based flow lives in DIP-0006 / DIP-0010 and
  the eventual `c0mpute-net` implementation.
- The `apps/coordinator` route shapes are preserved in git history if
  we ever want to revisit specific endpoint designs.

## Open questions

- **Dashboard data without a backend.** When per-plugin dashboards
  arrive (DIP-0008 future), they need *some* live data feed. Options:
  (a) browser-side libp2p light client; (b) CoinPay's own data APIs
  for receipt / reputation queries; (c) anyone-can-run aggregator
  nodes that expose read-only REST. Punt to whenever a per-plugin
  dashboard actually exists.
- **Light client UX.** A browser opening
  `c0mpute.com/transcode/jobs/<hash>` would be nicer if it could see
  the job's status without the user running a node. We can serve this
  through aggregator nodes that subscribe to gossipsub and expose a
  REST view, run by community + Profullstack on a best-effort basis.
  These aren't authoritative; they're caches.

## Out of scope

- Replacement protocols for every individual coordinator route. Most
  of them disappear (auth, sessions, user-specific lists). The few
  that need a p2p replacement (job submit, claim, complete, fail) are
  designed in DIP-0006 + the v1 PRD.
- Whether to keep the original PRD's data model (`docs/PRD.md`
  §13) as architectural reference. It's still a useful representation
  of what the system tracks; it just doesn't represent a hosted DB
  schema anymore.
