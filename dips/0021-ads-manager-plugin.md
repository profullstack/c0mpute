---
dip: 0021
title: "ads-manager — shared ad operations layer for all c0mpute ad plugins"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-06-04
updated: 2026-06-04
discussion:
implementation:
supersedes:
superseded-by:
---

## Summary

`ads-manager` is a first-party c0mpute service plugin that provides the
shared infrastructure all ad format plugins (ctv-ads, display-ads, audio-ads,
etc.) build on: campaign management, creative registry and transcoding,
CoinPay escrow lifecycle, OpenRTB auction primitives, publisher inventory
registry, audience segment management, and unified reporting. Format-specific
plugins declare `ads-manager` as a required dependency in their manifest and
call into it for everything that isn't format-specific.

This follows the same plugin composition pattern as `storage` (DIP-0012) and
`coinpay` (DIP-0007) — a shared primitive that vertical plugins consume rather
than each reinventing independently.

## User roles — buyer, seller, or both

Every c0mpute node running `ads-manager` participates as **publisher (seller)**,
**advertiser (buyer)**, or both at the same time:

| Role | What they do |
|---|---|
| **Publisher / seller** | Register ad inventory (channels, podcast shows, streams), set floor CPMs, choose ad formats, receive CoinPay settlement per impression |
| **Advertiser / buyer** | Create campaigns, upload creatives, set budgets and bids, target inventory, view reporting |

The two sides share the same plugin, the same escrow wallet, and the same
dashboard. A podcaster who also runs promotional campaigns for their own brand
manages everything from one place — `c0mpute ads` — without switching plugins
or accounts.

---

## Motivation

Without a shared layer, every ad format plugin would have to re-implement:
- Campaign CRUD, budget pacing, and CoinPay escrow management
- Creative upload, transcoding (via the `transcode` plugin), and CDN
  registration (via the `live-stream` plugin CDN)
- Publisher inventory registration and DHT announcement
- OpenRTB 2.6 BidRequest/BidResponse construction and second-price auction math
- Reporting aggregation and impression receipt indexing

That's 80% of the hard work done identically for every format. Centralising
it in `ads-manager` means `ctv-ads` is only ~400 lines of VAST-specific code
on top of a solid base.

There is also an important operational reason: advertisers want a single
dashboard. A brand running CTV, display, and audio ads on the c0mpute network
should see one campaign list, one budget, one reporting view — not three
separate plugins with three separate logins.

## Detailed design

### 1. What ads-manager owns

| Concern | ads-manager responsibility |
|---|---|
| Campaign lifecycle | create / pause / resume / end / clone |
| Budget & pacing | total budget, daily cap, CoinPay escrow create/topup/release |
| Creative registry | upload metadata, trigger transcode, register with CDN DHT |
| Inventory registry | publisher registers apps/channels/slots |
| Audience segments | segment definition, on-device cohort interface |
| OpenRTB primitives | BidRequest builder, BidResponse parser, auction (2nd price) |
| Reporting | impression receipt index, aggregation queries, export |
| Fraud signals | shared bloom filter for beacon replay detection |

### 2. What format plugins own

| Plugin | Format-specific additions |
|---|---|
| `ctv-ads` | VAST 4.x XML assembly, quartile beacon handling, SSAI hooks |
| `display-ads` | MRAID / HTML banner serving, viewability measurement |
| `audio-ads` | DAAST XML, audio quartile beacons |
| `native-ads` | OpenRTB native ad unit rendering, sponsored content format |

Format plugins import from ads-manager via the plugin IPC interface:
```
c0mpute ads campaign list       # ads-manager owns this
c0mpute ctv vast --pub <did>    # ctv-ads owns this, calls ads-manager auction
```

### 3. Campaign model

```json
{
  "id": "<uuid>",
  "name": "Brand Campaign Q3",
  "advertiser_did": "did:coinpay:user:abc123",
  "status": "active",
  "format": "ctv",
  "budget_usd": 10000,
  "budget_spent_usd": 1234.56,
  "daily_cap_usd": 500,
  "cpm_target": 12.50,
  "cpm_max": 20.00,
  "flight_start": "2026-07-01T00:00:00Z",
  "flight_end": "2026-09-30T23:59:59Z",
  "creatives": ["<creative_id_1>", "<creative_id_2>"],
  "targeting": {
    "genres": ["sports", "news"],
    "device_types": ["ctv"],
    "segments": ["auto_intenders"],
    "blocked_categories": ["adult", "gambling"]
  },
  "escrow_id": "<coinpay_escrow_id>",
  "created_at": "2026-06-01T00:00:00Z"
}
```

Campaigns live in the DHT (keyed by `SHA256("ads-campaign:" + id)`) and are
replicated to nodes with the `ads-manager` service role. Sensitive fields
(targeting detail, spend amounts) are encrypted with the advertiser's DID
public key; only the advertiser and authorized DSP nodes can read the full
record.

### 4. Creative registry

```bash
c0mpute ads creative upload --file ad-30s.mp4 --format ctv --duration 30
```

Steps:
1. File is content-addressed (BLAKE3 hash → `creative_id`)
2. `transcode` plugin produces format variants (1080p H.264, 720p H.264, HLS)
3. Variants announced to the CDN DHT (same mechanism as DIP-0019 live-stream
   segment distribution)
4. Creative metadata registered:
   ```json
   { "id": "<blake3_hash>", "format": "ctv", "duration_secs": 30,
     "mimes": ["video/mp4"], "variants": [...], "iab_categories": ["IAB2"],
     "owner_did": "did:coinpay:user:abc123", "verified_at": 1748995200 }
   ```
5. CDN nodes that cache the creative announce via gossip and earn per-GB served

### 5. Live-stream ad integration (SSAI)

The most compelling integration: a broadcaster uses the `live-stream` plugin
(DIP-0019) to stream content on the c0mpute network, and `ads-manager` /
`ctv-ads` automatically inject ads into the stream at defined break points.

```
Broadcaster → RTMP ingest → c0mpute ingest node
                                  │
                         HLS segmenter (FFmpeg)
                                  │
                      ┌───────────┴─────────────┐
                      │                         │
               Content segments           Ad break trigger
               (normal HLS)               (SCTE-35 cue or
                                           timed break config)
                                                 │
                                    ads-manager auction
                                                 │
                                    winning creative fetched
                                    from CDN node
                                                 │
                                    SSAI stitches ad segment(s)
                                    into HLS manifest
                                                 │
                               Viewer sees seamless ad break
                               (no separate VAST call from player)
```

Broadcasters opt in:
```bash
c0mpute stream create --ad-breaks 4 --break-duration 30 --floor-cpm 5.00
```

Revenue split for ad-supported live streams:
- Broadcaster: 55% of ad revenue
- Ad-serving nodes (SSP + measurement): 20%
- CDN nodes (content + creative): 15%
- Protocol fee: 10%

Viewers can also opt in to a **premium ad-free tier** by paying the
broadcaster directly via CoinPay micropayments — same stream, no ad segments
spliced in. This is a direct broadcaster-to-viewer payment with no
intermediary.

### 6. Publisher inventory registry

```bash
c0mpute ads publisher register \
  --name "Sports Network FAST" \
  --bundle com.sportsnetwork.tv \
  --content-genres sports,news \
  --floor-cpm 5.00 \
  --ad-formats ctv \
  --blocked-categories adult,gambling
```

Publisher profiles are DHT-announced:
`SHA256("ads-publisher:" + pub_did)` → inventory profile.

DSP nodes index the publisher DHT to know which inventory is available before
building campaigns. Publishers can update their profile (new floors, new
blocked categories) and the update propagates within one DHT TTL cycle.

### 7. Reporting

The `ads-manager` node indexes signed impression receipts (from measurement
nodes) and exposes aggregated reporting:

```bash
c0mpute ads report campaign <campaign_id> \
  --from 2026-07-01 --to 2026-07-31 \
  --breakdown daily,publisher,creative

# Output:
# date         publisher_did      creative_id  imps    completions  spend_usd  ecpm
# 2026-07-01   did:coinpay:…      abc123       12,400  9,920        155.00     12.50
# ...
```

Advertisers can also export raw receipt logs (JSONL) for their own
measurement pipeline. All receipts are signed by the measurement node DID —
independently verifiable without trusting the reporting system.

### 8. CLI surface

```bash
# Campaign management
c0mpute ads campaign create [--format ctv|display|audio] [--budget N] ...
c0mpute ads campaign list
c0mpute ads campaign status <id>
c0mpute ads campaign pause  <id>
c0mpute ads campaign resume <id>
c0mpute ads campaign clone  <id> --new-name "..."

# Creative management
c0mpute ads creative upload --file <path> --format ctv
c0mpute ads creative list
c0mpute ads creative status <id>   # transcoding progress, CDN coverage
c0mpute ads creative delete <id>

# Publisher management
c0mpute ads publisher register [options]
c0mpute ads publisher status
c0mpute ads publisher earnings --period 30d

# Reporting
c0mpute ads report campaign <id> [--breakdown daily|publisher|creative]
c0mpute ads report publisher       [--breakdown daily|campaign|creative]
c0mpute ads report receipts <id>   # export raw signed receipts

# Node operator
c0mpute ads node --enable          # opt into ads-manager service role
c0mpute ads node --status          # auctions served, earnings, uptime
```

### 9. Web UI (`c0mpute.com/ads`)

- **Advertiser dashboard**: campaign overview, budget pacing chart, top
  publishers, creative performance, CoinPay escrow balance
- **Publisher dashboard**: earnings, fill rate, eCPM trend, VAST tag
  generator (for ctv-ads), inventory profile editor
- **Creative library**: upload, transcoding status, CDN coverage map
- **Marketplace**: browse publisher inventory (similar to a self-serve ad
  exchange), filter by genre/device/floor CPM

## Alternatives considered

**Format plugins self-contained, no shared layer.** Simpler in the short
term but creates N copies of campaign management, N escrow integrations, N
reporting systems. Advertisers would need N dashboards. Rejected.

**Third-party ad server (FreeWheel, Google Ad Manager) as the backend.**
Defeats the decentralisation goal entirely. Kept as a "bridge mode" option
for v2 — let publishers who already have a GAM contract route Google demand
through c0mpute as an additional SSP, but the native path is always
c0mpute-first.

**Single monolithic "ads" plugin.** Puts CTV-specific VAST logic, display
banner serving, audio DAAST, and native format rendering all in one binary.
Results in a huge codebase where every node has to understand every format.
The ads-manager + format-plugin split keeps format nodes lean.

## Migration & rollout

This plugin ships in lockstep with `ctv-ads` (DIP-0020), which is the first
consumer. Rollout mirrors the ctv-ads phases:

1. Campaign CRUD + escrow lifecycle (needed by ctv-ads v0.1)
2. Creative registry + CDN integration (ctv-ads v0.2)
3. Reporting + receipt indexing (ctv-ads v0.3)
4. Publisher inventory DHT (ctv-ads v0.4)
5. SSAI live-stream integration (ctv-ads v0.5 / live-stream v0.5)
6. Multi-format support (display-ads, audio-ads as separate plugins)

## Open questions

- **Receipt storage.** Impression receipts accumulate fast at scale (millions
  per day). Nodes storing the full receipt log need significant disk. Should
  receipt storage be handled by the `storage` plugin (DIP-0012) with
  erasure coding, or kept in a lightweight append-only log per node?
- **Pacing algorithm.** Budget pacing (spend $500/day evenly, not all in
  the first auction of the day) is a non-trivial distributed systems problem.
  Each DSP node paces independently with a target impression rate — but what
  happens when a node restarts and loses its pacing state? Need a recovery
  path.
- **Cross-format campaigns.** An advertiser running both CTV and audio ads
  under one budget — how does the escrow split? Does ads-manager allocate
  sub-budgets per format, or is it first-come-first-served across formats?

## Out of scope

- Programmatic guaranteed / private marketplace (PMP) deals
- Header bidding (client-side parallel auctions for web display)
- Attribution beyond impression (post-click conversion tracking)
- Advertiser DMP / data management (audience segment computation is
  publisher-side and on-device; ads-manager doesn't store user behavioral data)
