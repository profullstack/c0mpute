---
dip: 0020
title: "ctv-ads — decentralized Connected TV ad marketplace on c0mpute"
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

`ctv-ads` is a c0mpute plugin that turns the network into a decentralized
Connected TV (CTV) ad marketplace. Publishers (streaming apps on Roku, Apple
TV, Fire TV, Chromecast, Smart TVs, FAST channels) route ad requests through
c0mpute nodes instead of Google Ad Manager, Magnite, or Index Exchange.
Advertisers submit campaigns and bids via a standard OpenRTB-compatible
interface. c0mpute nodes run the auction, serve VAST-compliant video ad
creatives via the same p2p CDN as the live-stream plugin (DIP-0019), and
produce cryptographically signed impression receipts that eliminate ad fraud
by design. CoinPay (DIP-0007) settles payments directly from advertiser
escrow to publisher — no SSP, no DSP, no ad-server middleman taking 30–70%
of spend.

## Motivation

CTV is the fastest-growing ad channel ($30B+ US spend in 2026) and also one
of the most broken:

1. **Fee stacking.** A typical CTV dollar travels through a DSP, an SSP, an
   ad server, a measurement vendor, and a data broker before a cent reaches
   the publisher. Each hop takes 10–30%. Publishers often net $0.30–0.50 on
   the dollar.

2. **Impression fraud.** IAS, DoubleVerify, and Pixalate estimate 20–35% of
   programmatic CTV impressions are invalid — bot traffic, device spoofing,
   domain laundering. There is no ground truth because every participant in
   the chain has an incentive to over-count.

3. **Opacity.** Advertisers cannot see which publisher, device, or app their
   ad appeared on. Publishers cannot see who bid on their inventory or why
   they lost. "Walled gardens" (Google, Amazon) give neither side the full
   picture.

4. **No permissionless access.** Independent streaming channels (FAST
   channels, indie apps, foreign-language content) can't access premium
   demand because getting into Google or Magnite requires manual review,
   minimum revenue commitments, and geographic restrictions.

c0mpute is structurally positioned to fix all four: the network already has
p2p CDN (DIP-0019), identity and reputation (DIP-0007), and verifiable
receipts (transcode auction). The CTV ad flow is just a real-time auction
followed by creative delivery and impression verification — all shapes the
network already knows.

## Detailed design

### 1. Roles

| Role | Who | What they do |
|---|---|---|
| **Publisher node** | Streaming app / FAST channel operator | Registers inventory, receives VAST response, fires beacons |
| **SSP node** | c0mpute worker | Accepts bid requests from publishers, runs OpenRTB auction among DSPs |
| **DSP node** | c0mpute worker | Holds advertiser campaigns, submits bids, manages pacing |
| **Creative CDN node** | c0mpute worker | Stores and serves video ad creatives (VAST XML + MP4/HLS) |
| **Measurement node** | c0mpute worker | Receives impression beacons, signs verified receipts, aggregates reporting |
| **Advertiser** | Brand / agency | Uploads creatives, sets targeting, funds CoinPay escrow |

A single c0mpute node can serve multiple roles (e.g., SSP + measurement).

### 2. Ad request flow

```
Streaming player (Roku / Fire TV / Smart TV / web app)
  │
  │  1. Ad break starts → player calls VAST URL:
  │     GET https://node.c0mpute.com/ctv/vast?pub=<pub_did>&slot=preroll&...
  │
  ▼
SSP node (c0mpute worker, ctv-ads plugin, SSP role)
  │
  │  2. Build OpenRTB 2.6 BidRequest, broadcast via gossip topic
  │     c0mpute/ctv/bid-request/v1
  │
  ▼
DSP nodes (all nodes with campaigns matching inventory)
  │
  │  3. Each eligible DSP responds with BidResponse within TTL (default 120ms)
  │     c0mpute/ctv/bid-response/v1
  │
  ▼
SSP node (auction)
  │
  │  4. Second-price auction, winner selected, VAST XML assembled
  │     Winner notified; losing DSPs notified (optional)
  │
  ▼
Player receives VAST XML
  │
  │  5. Player fetches video creative from creative CDN node
  │     MP4 served from p2p swarm (same transport as DIP-0019 live-stream)
  │
  │  6. Player fires impression beacons as video plays:
  │     - impression (0% watched)
  │     - firstQuartile (25%)
  │     - midpoint (50%)
  │     - thirdQuartile (75%)
  │     - complete (100%)
  │
  ▼
Measurement node
  │
  │  7. Validates beacons (device attestation, timing, anti-replay)
  │     Signs impression receipt with node's Ed25519 key (via CoinPay DID)
  │
  ▼
CoinPay settlement (epoch-based, per DIP-0007)
     Advertiser escrow → publisher DID (70%) + SSP node (15%) + CDN node (10%) + measurement node (5%)
```

### 3. VAST integration

The SSP node exposes a standard VAST 4.x endpoint compatible with all CTV
players:

```
GET /ctv/vast
  ?pub=<publisher_did>
  &slot=<preroll|midroll|postroll>
  &app=<app_bundle_id>
  &device_type=<ctv>
  &content_id=<optional content CID>
  &w=1920&h=1080
  &max_dur=30
```

Response: a VAST 4.x XML document with:
- `<MediaFile>` pointing to the winning creative on the CDN node
- `<Impression>` tracking pixel pointing to the measurement node
- `<TrackingEvents>` for quartile beacons
- `<Extensions>` containing the signed auction receipt (publisher can verify
  they got the right clearing price)

Publishers drop this URL into their player's ad tag slot — identical to
how they'd integrate with FreeWheel, Google Ad Manager, or SpotX. No SDK
required. VAST is the lingua franca of CTV; every player on every device
speaks it.

### 4. OpenRTB 2.6 bid request

The SSP node constructs a standard BidRequest with c0mpute-specific
extensions:

```json
{
  "id": "<uuid>",
  "imp": [{
    "id": "1",
    "video": {
      "mimes": ["video/mp4"],
      "minduration": 15,
      "maxduration": 30,
      "protocols": [2, 3, 5, 6],
      "w": 1920,
      "h": 1080,
      "linearity": 1,
      "skip": 0,
      "placement": 1
    },
    "bidfloor": 5.00,
    "bidfloorcur": "USD",
    "ext": {
      "c0mpute": {
        "pub_did": "did:coinpay:user:abc123",
        "slot": "preroll",
        "content_genre": "sports"
      }
    }
  }],
  "site": { "page": "...", "publisher": { "id": "..." } },
  "app": { "bundle": "com.tubi.tv", "storeurl": "..." },
  "device": {
    "ua": "...",
    "devicetype": 3,
    "make": "Roku",
    "model": "Express"
  },
  "user": {
    "ext": {
      "c0mpute": {
        "audience_segments": ["sports_fans", "auto_intenders"],
        "device_fingerprint_hash": "<sha256>"
      }
    }
  },
  "at": 2,
  "tmax": 120,
  "cur": ["USD"]
}
```

**Privacy**: `user.id` is never set. Audience segments are computed locally
on the publisher's device using a federated segment graph (see §6) and only
segment labels — never raw behavioral data — are shared in the bid request.

### 5. Creative registry (DHT-backed)

Advertisers register creatives before campaigns go live:

```bash
c0mpute ctv creative upload --file ad-30s.mp4 --title "Brand Campaign Q3"
# → content-addresses the file, announces to DHT, returns creative_id
```

Creatives are stored and served by Creative CDN nodes using the same
segment-swarming transport as DIP-0019. DHT key:
`SHA256("ctv-creative:" + creative_id)` → node list.

VAST `<MediaFile>` URLs resolve to the local CDN node serving the creative
(transparent to the player):
```
https://node.c0mpute.com/ctv/creative/<creative_id>/ad.mp4
```

CDN nodes earn per-GB served for creative delivery.

### 6. Privacy-preserving targeting

c0mpute CTV targeting does not require third-party cookies or device ID
tracking:

- **Contextual segments**: publisher declares content genre, app category,
  time-of-day — no user data needed.
- **On-device cohorts**: the c0mpute client (or a publisher SDK) computes
  audience segment membership locally using a privacy-preserving segment
  model (similar to Google's Topics API but open-source and verifiable).
  Only segment labels are shared — `["sports_fans", "auto_intenders"]` —
  never the underlying behavioral signals.
- **Publisher first-party data**: publishers can hash their own user IDs and
  pass an encrypted segment vector to DSPs that have a data-sharing
  agreement. All encryption uses the same X25519 key infrastructure from
  DIP-0018.
- **No fingerprinting**: device characteristics are hashed before leaving
  the device; the hash is used only for frequency capping, never for
  cross-publisher tracking.

### 7. Impression verification (anti-fraud)

The measurement node validates each beacon against three checks:

| Check | Method |
|---|---|
| **Timing integrity** | Quartile beacons must arrive in sequence with plausible timing (firstQuartile ≥ 7.5s after impression for a 30s ad) |
| **Device attestation** | Publisher node signs the ad session with its CoinPay DID key; measurement node verifies sig before accepting beacons |
| **Replay prevention** | Beacon nonce is checked against a short-lived bloom filter; duplicate nonces rejected |

Verified impressions produce a signed receipt:

```json
{
  "v": 1,
  "auction_id": "<uuid>",
  "creative_id": "<cid>",
  "pub_did": "did:coinpay:user:abc123",
  "adv_did": "did:coinpay:user:def456",
  "event": "complete",
  "cleared_cpm": 12.50,
  "verified_at": 1748995200,
  "sig": "<measurement_node_Ed25519_sig>"
}
```

This receipt is the settlement trigger for CoinPay. Advertisers only pay for
receipts with `event: "complete"` (or whatever completion threshold they set
— 50% minimum is common). Impression fraud simply cannot produce valid
signed receipts without compromising a measurement node's DID key — which
would be visible on-chain and slash the node's reputation.

### 8. Campaign management CLI

```bash
# Advertiser
c0mpute ctv campaign create \
  --name "Brand Q3" \
  --budget 10000 --currency USD \
  --cpm-target 12.50 \
  --creative <creative_id> \
  --targeting genre=sports,device=ctv \
  --flight-start 2026-07-01 --flight-end 2026-09-30
# → funds CoinPay escrow, announces campaign to DSP nodes

c0mpute ctv campaign status <campaign_id>   # impressions, spend, completion rate
c0mpute ctv campaign pause  <campaign_id>
c0mpute ctv campaign resume <campaign_id>

# Publisher
c0mpute ctv publisher register \
  --app-name "Sports Network" \
  --bundle com.sportsnetwork.tv \
  --content-genres sports,news \
  --floor-cpm 5.00
# → registers pub DID + inventory profile in DHT

c0mpute ctv publisher status               # fill rate, eCPM, earnings today

# Creative management
c0mpute ctv creative upload --file ad.mp4
c0mpute ctv creative list
c0mpute ctv creative delete <creative_id>

# Node operator
c0mpute ctv ssp --enable    # run SSP + auction role on this node
c0mpute ctv dsp --enable    # run DSP role (requires loaded campaigns)
c0mpute ctv cdn --enable    # run creative CDN role
c0mpute ctv measure --enable # run measurement/verification role
```

### 9. Payment model

| Flow | Mechanism |
|---|---|
| Advertiser funds campaign | `coinpay escrow create --campaign <id> --amount <n> --token USDC` |
| Impression verified | Measurement node signs receipt |
| Epoch settlement (every 300s) | SSP node aggregates receipts, submits batch claim to CoinPay |
| Disbursement | Publisher 70% / SSP node 15% / CDN node 10% / measurement node 5% |
| Unspent budget | Returned to advertiser at campaign end |

CPM floor and clearing price are denominated in USD, settled in USDC (or
whatever token the publisher accepts per their DIP-0007 payment preference).

### 10. FAST channel support

Free Ad-Supported Streaming TV (Tubi, Pluto TV, Samsung TV Plus, etc.) is
the highest-growth CTV inventory segment. These channels run 4–8 ad pods
per hour of content, each with 2–4 spots. c0mpute is especially well-suited
because:

- FAST channel operators are often small / mid-size and get squeezed hardest
  by SSP fees
- They have first-party audience data (viewing history) they can't monetize
  because they're locked out of walled-garden demand
- c0mpute gives them direct access to advertiser demand without a Magnite
  account minimum

FAST channel support requires no extra configuration — the publisher just
sets `content_type: "fast"` in their inventory profile.

### 11. CoinPay DID integration

- **Publisher DID**: `did:coinpay:user:<id>` — publisher registers inventory
  profile linked to their DID; earnings aggregate to the linked wallet
- **Advertiser DID**: campaign source of truth; escrow bound to advertiser DID
- **Node reputation**: SSP/DSP/CDN/measurement nodes earn `ctv.impressions_served`,
  `ctv.fill_rate`, `ctv.fraud_caught` counters on their DIDs. Low fill rate or
  high fraud rate triggers reputation penalty and removal from auction pool
- **Advertiser reputation**: brands with high dispute rates (claiming
  non-delivery on receipts that check out) have future campaigns require
  higher escrow multiples

### 12. Web UI (`c0mpute.com/ctv`)

- **Advertiser portal**: campaign creation wizard, creative uploader, real-time
  dashboard (impressions, completion rate, spend pace, eCPM)
- **Publisher portal**: VAST tag generator (copy/paste into player), earnings
  dashboard, fill-rate by slot, eCPM by genre
- **Marketplace**: browse available publisher inventory (app name, genre,
  estimated monthly impressions, floor CPM) — like a p2p ad exchange listing

### 13. Protocol message types

Extends gossip protocol with four new message types:

| Type | Value | Purpose |
|---|---|---|
| `CTV_BID_REQUEST`  | `0x40` | SSP broadcasts OpenRTB BidRequest |
| `CTV_BID_RESPONSE` | `0x41` | DSP unicasts BidResponse to SSP |
| `CTV_WIN_NOTIFY`   | `0x42` | SSP notifies winning DSP (for billing/pacing) |
| `CTV_BEACON`       | `0x43` | Player impression/quartile/complete event |

Bid responses are unicasted (not gossipped) to avoid leaking competitor bid
prices.

### 14. Security considerations

| Threat | Mitigation |
|---|---|
| Fake impressions | Device-attested sessions + timing checks + replay bloom filter; only signed receipts trigger payment |
| Domain spoofing (laundering) | Publisher DID is verified on registration; `app.bundle` in BidRequest verified against publisher's registered profile |
| Bid shading / price manipulation | Second-price auction math is deterministic and logged; any node can audit the clearing price from the auction receipt |
| DSP data leakage | Bid responses are unicasted to SSP, never broadcast; audience segment labels are one-way hashes |
| Creative malware | Creatives are content-addressed (SHA-256); CDN nodes serve only registered creatives that passed an integrity check at upload |
| SSP collusion (stuffing fake inventory) | Multiple independent measurement nodes cross-verify; publisher DID reputation slashed on fraud detection |
| Escrow drain | Settlement batch claims are bounded per campaign per epoch; CoinPay enforces max payout rate |

## Alternatives considered

**Integrate with an existing SSP (Magnite, Index Exchange).** Defeats the
purpose — still centralised, still high fees, still opaque. Worth building
a bridge adapter for v2 (let existing DSPs bid into c0mpute auctions via
OpenRTB passthrough), but the native path is the product.

**On-chain auction (smart contract per auction).** Latency is the killer —
CTV bid deadline is 120ms; Ethereum blocks are 12s; even Solana is ~400ms.
Off-chain auction with on-chain settlement (what we're doing here) is the
only viable architecture.

**Google Ad Manager integration.** Google charges 20%+ and requires
exclusivity for certain inventory types. Incompatible with the
censorship-resistance story.

**Content-ID / contextual only (no audience targeting).** Simpler but leaves
most advertiser value on the table. Contextual targeting is the floor, not
the ceiling. Publishers need CPMs competitive with walled-garden demand to
switch.

**VPAID instead of VAST.** VPAID is deprecated, blocked on many CTV devices,
and requires a JavaScript runtime the TV OS may not provide. VAST 4.x with
server-side ad insertion (SSAI) is the industry direction.

## Migration & rollout

1. **v0.1 — VAST endpoint + basic auction.** SSP node serves VAST XML. DSP
   nodes submit bids. Second-price auction. Creative served from a single
   CDN node (no swarm yet). Manual CoinPay settlement. Proves end-to-end flow
   with a test Roku channel.
2. **v0.2 — Creative CDN swarm.** Creatives distributed via DIP-0019 segment
   transport. Multiple CDN nodes. Per-GB CDN earnings.
3. **v0.3 — Measurement nodes + signed receipts.** Impression beacon
   validation. Anti-fraud checks. Signed receipts trigger automated
   CoinPay settlement.
4. **v0.4 — Campaign management + publisher portal.** Full CLI + web UI for
   advertisers and publishers. DHT-backed inventory registry.
5. **v0.5 — Privacy-preserving targeting.** On-device cohort segments.
   First-party data support. Frequency capping via hashed device ID.
6. **v1.0 — FAST channel program.** Outbound integration to recruit FAST
   channel operators. Direct sales motion.

## Open questions

- **Bid timeout.** 120ms is standard programmatic CTV. On a gossip network
  with geographically distributed nodes, p99 round-trip may exceed this.
  Do we need regional SSP node clustering, or can we relax to 200ms without
  losing fill?
- **SSAI vs. client-side VAST.** Server-side ad insertion (SSAI) stitches
  the ad into the video stream server-side, avoiding ad blockers and improving
  ViewThrough measurement. Should the live-stream plugin (DIP-0019) expose
  SSAI hooks for ctv-ads to splice into? This would be a significant but
  valuable integration.
- **Content moderation.** Who decides what ads are acceptable? The network
  is permissionless but publishers will want controls (no competitors,
  no adult content, no political ads on kids apps). Proposal: publisher
  declares a `blocked_categories` list in their inventory profile (IAB
  content taxonomy); DSPs tag creatives with categories at upload.
- **Dispute resolution.** Advertiser says the creative didn't play; publisher
  says it did. The signed receipt is the ground truth — but what if the
  measurement node was compromised? Need a multi-measurement-node quorum
  for high-value campaigns.
- **International.** CTV CPMs vary wildly by market ($3 in LatAm vs. $20+ in
  the US). Does the floor CPM system need market-aware defaults?
- **Regulatory.** CTV advertising is subject to FTC disclosures (ad
  disclosure overlays), COPPA (no behavioral targeting on kids content), and
  various state privacy laws. The protocol must let publishers enforce these
  at the inventory level. Worth a follow-on DIP before v1.0 launch.

## Out of scope

- Display / banner ads (CTV is video-only; banner ads have different
  economics and a separate plugin would make more sense)
- Linear TV / cable ad insertion
- Podcast / audio-only ad serving
- Attribution / conversion tracking beyond impression (click-through
  attribution on CTV is a separate problem)
- Demand-side buying UI (use the CLI; a full DSP dashboard is a separate
  product)
- Programmatic guaranteed / private marketplace (PMP) deals (v2+)
