# DIP-0023 — Podcast Ads Plugin

| Field       | Value                          |
|-------------|-------------------------------|
| DIP         | 0023                          |
| Title       | Podcast Ads Plugin            |
| Status      | Draft                         |
| Author      | Profullstack                  |
| Created     | 2026-06-04                    |
| Requires    | DIP-0007, DIP-0021, DIP-0022  |

---

## 1. Overview

`podcast-ads` is the audio ad format layer for the c0mpute network. It sits on top of `ads-manager` (DIP-0021) and hooks into the `podcasting` plugin (DIP-0022) to enable fully decentralised, programmatic audio advertising — with no platform cut beyond the configurable protocol split.

Every c0mpute node running `podcast-ads` can act as **publisher** (selling ad inventory in their podcast episodes), **advertiser** (running audio campaigns), or both simultaneously. The `ads-manager` plugin provides the shared auction, escrow, and reporting infrastructure; `podcast-ads` owns the audio-specific format details.

---

## 2. Roles

### 2.1 Publisher (seller)

A podcaster installs `podcast-ads` to monetise their show. They:

- Declare available ad slots (preroll, midroll, postroll) with floor CPM and duration constraints.
- Choose between **dynamic ad insertion (DAI)** — ads stitched into episode audio on serve — or **host-read slots** — pre-recorded reads matched to campaigns manually.
- Optionally enable **Podcasting 2.0 `<podcast:value>` injection** so listeners using value-for-value apps (Fountain, Breez) pay the publisher directly per minute, with ad revenue treated as an additional layer on top.
- View per-episode impression reports and receive CoinPay settlement automatically at each epoch.

### 2.2 Advertiser (buyer)

A brand or individual installs `podcast-ads` to run audio campaigns. They:

- Create campaigns in `ads-manager` specifying budget, bid CPM, target categories, geotargeting, and scheduling.
- Upload audio creatives (MP3/AAC) which are transcoded and DHT-announced via the `transcode` and `storage` plugins.
- Track impressions, completion rates, and spend in real time via `ads-manager`'s unified reporting dashboard.
- Funds are held in CoinPay escrow; released per verified impression receipt.

---

## 3. Protocol

### 3.1 Gossip message types

| Type | Hex  | Name                  | Direction              |
|------|------|-----------------------|------------------------|
| 0    | 0x50 | `AUDIO_BID_REQUEST`   | Publisher → network    |
| 1    | 0x51 | `AUDIO_BID_RESPONSE`  | Advertiser nodes → pub |
| 2    | 0x52 | `AUDIO_WIN_NOTIFY`    | Publisher → winner     |
| 3    | 0x53 | `AUDIO_BEACON`        | Listener → network     |

### 3.2 DAAST endpoint

```
GET /podcast-ads/daast
  ?pub=<did>
  &slot=<preroll|midroll|postroll>
  &dur=<15|30|60>
  &cat=<IAB category>
  &ep=<episode_id>
  &listener=<did>          # optional, for frequency capping
```

Returns DAAST 1.0 XML. The `podcasting` plugin's episode serve path calls this endpoint when DAI is enabled, stitches the returned audio into the stream, and fires impression beacons back.

### 3.3 Impression tracking

Listeners (via the `podcasts` consumer plugin) fire beacons at:

- `start` — first 2 seconds played
- `firstQuartile` — 25%
- `midpoint` — 50%
- `thirdQuartile` — 75%
- `complete` — 97%+ played

Beacons are gossip messages (`0x53 AUDIO_BEACON`) signed with the listener's CoinPay DID key. Each node running `podcast-ads` validates timing, checks the bloom filter for replays, and contributes the receipt to the `ads-manager` settlement batch.

### 3.4 Dynamic Ad Insertion (DAI)

When `dai_enabled = true`, the `podcasting` plugin calls `podcast-ads`'s DAI hook at episode serve time. The hook:

1. Calls the DAAST endpoint to win an auction.
2. Fetches the creative from the p2p swarm (same transport as `live-stream`, DIP-0019).
3. Passes the audio to the `transcode` plugin for format-matching.
4. Returns a spliced audio stream to the `podcasting` episode handler.

DAI is optional. Publishers can instead use host-read slots (recorded manually) or static feed-level `<podcast:value>` splits without any audio stitching.

### 3.5 Podcasting 2.0 integration

When `value_block_injection = true`, `podcast-ads` patches the podcasting plugin's RSS feed generator to insert a `<podcast:value>` block splitting per-episode revenue between the podcaster, the ad-serving nodes, and the protocol:

```xml
<podcast:value type="lightning" method="keysend">
  <podcast:valueRecipient name="Podcaster"   type="node" address="..." split="60" />
  <podcast:valueRecipient name="Ad Network"  type="node" address="..." split="20" />
  <podcast:valueRecipient name="c0mpute"     type="node" address="..." split="10" />
  <!-- advertiser performance rebate handled off-feed via CoinPay -->
</podcast:value>
```

---

## 4. Revenue split

Default basis points (configurable per publisher):

| Recipient            | BPS   | Pct  |
|----------------------|-------|------|
| Publisher/podcaster  | 6 000 | 60 % |
| Auction/serving nodes| 2 000 | 20 % |
| c0mpute protocol     | 1 000 | 10 % |
| Advertiser rebate    | 1 000 | 10 % |

The advertiser rebate is returned to the campaign's escrow wallet when completion rate exceeds the campaign's target threshold (default 75 % complete rate). Campaigns that underperform forfeit the rebate; it rolls into the protocol split.

---

## 5. Settlement

`podcast-ads` does not run its own settlement. It produces signed impression receipts and hands them to `ads-manager`, which batches them into CoinPay settlement epochs (default 5 minutes). Publishers receive USDC or Lightning payments automatically; no manual withdrawal needed.

---

## 6. Anti-fraud

- **Replay protection**: bloom filter on `(beacon_type, episode_id, listener_did, ad_id)` tuples with configurable TTL (default 300 s). Replayed beacons are dropped.
- **Timing validation**: `firstQuartile` beacon must arrive ≥ 25 % of the ad duration after `start`; similarly for midpoint, thirdQuartile, complete.
- **Listener DID signature**: beacons are signed with the listener's CoinPay DID Ed25519 key. Unsigned beacons are dropped.
- **Rate limiting**: no more than 1 impression per `(listener_did, ad_id)` per episode play.

---

## 7. CLI surface

```
c0mpute podcast-ads
  publisher
    slots list [--show <show_id>]
    slots add  --show <show_id> --type <preroll|midroll|postroll> --floor-cpm <usd> --dur <s>
    slots rm   --show <show_id> --slot-id <id>
    earnings   [--show <show_id>] [--from <date>] [--to <date>]

  advertiser
    campaign list
    campaign new  --name <str> --budget <usd> --bid-cpm <usd> --cat <iab> [--dur <s>]
    campaign pause  <campaign_id>
    campaign resume <campaign_id>
    creative upload --campaign <id> --file <path>
    report   <campaign_id> [--from <date>] [--to <date>]
```

---

## 8. Dependencies

| Plugin        | Version  | Why                                              |
|---------------|----------|--------------------------------------------------|
| `ads-manager` | ≥ 0.0.1  | Campaign CRUD, OpenRTB auction, CoinPay escrow   |
| `podcasting`  | ≥ 0.0.1  | DAI hook, RSS feed injection, episode serve path |
| `coinpay`     | ≥ 0.2.0  | DID identity, per-listen micropayments           |

Optional (pulled in transitively via `ads-manager`):
- `transcode` — needed for DAI audio stitching
- `storage` — creative CDN and impression receipt log persistence

---

## 9. First-party Rust crate

Target: `node/crates/c0mpute-podcast-ads`

Scaffolding deferred until `ads-manager` and `podcasting` crates reach v0.1. The DAAST XML assembler, beacon handler, and DAI hook are the three entry points.
