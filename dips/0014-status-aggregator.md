---
dip: 0014
title: "Public /status page + status-aggregator service"
status: Accepted
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-05-03
discussion:
implementation: apps/web/src/app/status (page + /api/status); aggregator service deferred
supersedes:
superseded-by:
---

## Summary

c0mpute.com publishes a public `/status` page showing aggregate
network health: workers online, jobs in flight, jobs/24h, average
latency. **No private data is ever displayed** — no peer-ids, no
DIDs, no IPs, no individual jobs, no customer references.

The data comes from a **status-aggregator** service that we run as a
sibling Railway deployment. The aggregator is a c0mpute node booted
in `verifier` mode that subscribes to `c0mpute/cap/v1` and
`c0mpute/jobs/*` gossipsub topics, aggregates counts in memory, and
exposes a JSON endpoint over Railway's private network. The
c0mpute.com Next.js app proxies that JSON at `/api/status` (15s
cache) and renders `/status` on top.

For this commit the aggregator is **not yet deployed** — `/status`
returns placeholder zeros. The page works visually; the live numbers
land once we cut a `c0mpute-status-aggregator` service.

## Motivation

Three uses for a public status surface:

1. **Trust-by-numbers.** "Is c0mpute alive? Are jobs being run? How
   many workers?" Real numbers answer better than marketing copy.
2. **Operator self-service.** Workers can check that the network
   sees them at all without spelunking gossipsub logs.
3. **Public commitment.** Once we publish numbers, we have to keep
   them honest. That's a forcing function for keeping the network
   healthy rather than papering over outages.

Constraint: **never expose private data.** Three reasons —
adversarial scraping (malicious actor learns network topology),
customer privacy (job specs may contain confidential workloads), and
worker privacy (operators don't want their IPs published).

## Detailed design

### What the page shows

```
[ network ]
  workers online           312
  jobs in flight            42
  jobs completed (24h)   1,847
  avg job latency        14.3s

[ workers by role ]
  storage                 198
  transcode               142
  gateway                  47
  verifier                  9
```

That's it. No drill-downs. No charts of individual jobs. No
maps of where workers are.

### What the page does NOT show (privacy model)

Explicitly forbidden — never sneak these in even by accident:

- Individual peer-ids, public keys, or DIDs
- IP addresses, multiaddrs, geographic location finer than ~country
  (and we don't even ship country today)
- Individual job ids, manifests, inputs, outputs, or hashes
- Customer / buyer identifiers of any kind
- Worker reputation scores or rankings (those go on the
  `c0mpute trust inspect <did>` CLI surface, queried by DID — not
  on a public scrape-friendly page)

The `/api/status` JSON has a contract that mirrors this — only
aggregate counts and an `ok` bool.

### Aggregator architecture

```
┌──────────────────────── c0mpute.com (Railway) ─────────────────────────┐
│                                                                         │
│  apps/web (Next.js)                                                     │
│   /status            ─┐                                                 │
│   /api/status        ─┼─▶ proxy to aggregator                           │
│                       │                                                 │
└───────────────────────┼─────────────────────────────────────────────────┘
                        │  internal Railway DNS (status.railway.internal)
                        ▼
┌──────────────────── status-aggregator (Railway) ───────────────────────┐
│                                                                         │
│   c0mpute --status-aggregator-mode                                      │
│     subscribes:   c0mpute/cap/v1                                        │
│                   c0mpute/jobs/*                                        │
│                   c0mpute/heartbeat/v1                                  │
│     in-memory:    HashMap<PeerId, last_seen>                            │
│                   atomic counters for jobs in flight / 24h              │
│     HTTP JSON:    GET / -> StatusPayload                                │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

The aggregator is just a c0mpute node with a `--status-aggregator-mode`
flag (lands in a follow-up commit). It uses the existing capability
registry + a new `JobTracker` that watches `JobOffer` / `JobAccept` /
`JobReceipt` topic flow and maintains running counts.

It's not authoritative for anything — it's a **read-only listener**
on public gossipsub topics. If it dies, the network keeps running;
only `/status` goes stale.

### JSON contract

```json
{
  "ok": true,
  "generated_at": "2026-05-03T19:42:00Z",
  "network": {
    "workers_online": 312,
    "workers_with_role": {
      "storage": 198,
      "transcode": 142,
      "gateway": 47,
      "verifier": 9
    },
    "jobs_in_flight": 42,
    "jobs_completed_24h": 1847,
    "avg_job_latency_seconds": 14.3
  },
  "source": "aggregator"
}
```

Frozen at this shape. Any future field must be reviewed for privacy.

### Wiring on c0mpute.com

Next.js app reads `STATUS_AGGREGATOR_URL` env var. When set, proxies
to it. When unset (today), returns a stub payload with zeros and
`source: "stub"`. Page shows a clear "placeholder · aggregator not
deployed" notice when stub.

### Caching

- `/api/status` Next.js route: 15s cache (`s-maxage=15`)
- Page: `revalidate = 30` (Next.js incremental static regeneration)
- Aggregator side: counts updated continuously in memory, served on
  every GET

That keeps load on the aggregator under one hit per 15s per CDN
edge, regardless of traffic.

## Alternatives considered

**Browser-side libp2p.** Have the page itself open a libp2p
WebTransport / WebRTC connection to the network. Massively
overcomplicated, fragile, and exposes more network shape to scrapers
than a single aggregated JSON does.

**Per-job public ledger / receipt log.** Tempting for transparency
but conflicts with customer privacy. Public attestations belong on
CoinPay's surface, queried by DID, not on a scrape-friendly page.

**No public status at all.** Loses the trust-by-numbers value. The
status page is cheap and the constraint to keep it aggregate-only is
straightforward to enforce.

**Store metrics in Postgres / a database.** Reintroduces the central
backend rejected in DIP-0011. The aggregator's in-memory state is
fine — we don't need historical metrics for the public page (24h
counter resets if the aggregator restarts; that's acceptable for a
real-time status surface).

## Migration & rollout

Phase 1 (this commit):
- `/status` page + `/api/status` route in apps/web. Serves a stub
  payload until the aggregator is deployed.
- DIP locks the JSON contract + privacy model.

Phase 2 (follow-up):
- `c0mpute --status-aggregator-mode` flag in c0mpute-cli.
- Implement the JobTracker (subscribes to JobOffer/Accept/Receipt
  on c0mpute/jobs/*, maintains counters).
- Compose with the existing Registry for worker counts.
- HTTP JSON endpoint at `/`.

Phase 3:
- Deploy on Railway as a sibling to c0mpute.com. Use Railway's
  private network for the proxy fetch.
- Set `STATUS_AGGREGATOR_URL` on the c0mpute.com service.

## Open questions

- **Anti-scraping.** Even with aggregate-only data, do we want to
  rate-limit `/api/status`? Probably yes at the edge. Cloudflare /
  Railway both support this trivially.
- **Multiple aggregators.** Eventually we'd want >1 aggregator for
  redundancy. The c0mpute.com proxy can fall back across them. Out
  of scope for v1.
- **Historical data.** Do we want sparkline-style "workers online
  over time" charts? Adds complexity (need persistent storage) for
  modest value. Punt unless someone asks.

## Out of scope

- Drill-down per-worker or per-job views. Forbidden by the privacy
  model.
- Public job receipts / attestations — that's a CoinPay surface.
- Authentication (the page is fully public).
- Real-time websocket updates — 15-30s cache is fine.
