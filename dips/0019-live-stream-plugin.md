---
dip: 0019
title: "live-stream — p2p HLS live streaming via BitTorrent-style segment swarming"
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

`live-stream` is a first-party c0mpute plugin that turns the network into a
decentralized live-streaming CDN. A broadcaster pushes RTMP or SRT to any
c0mpute ingest node; FFmpeg segments the stream into HLS chunks (`.ts` +
`.m3u8`); nodes swarm the segments BitTorrent-style; viewers pull a standard
HLS playlist from the nearest node and watch in any HLS-capable player (VLC,
Safari, mpv, ffplay, hls.js). No Twitch. No YouTube. No Akamai. The more
viewers there are, the more bandwidth the swarm provides — the load scales up
with demand instead of against it.

This is the open-source successor to what Acestream / SopCast proved was
technically viable for live sports circa 2010: BitTorrent-style live delivery
is real, it works at scale, and nobody has shipped it as an open, permissionless
protocol backed by a compute marketplace. We are doing that.

## Motivation

Three overlapping demand signals:

1. **Live sports and events are the hardest CDN workload.** A single Champions
   League match can spike to millions of concurrent viewers for 90 minutes and
   then drop to zero. Traditional CDNs charge for that peak. A swarm inverts
   the cost curve: every viewer is also a seeder; peak viewers = peak bandwidth
   supply.

2. **Censorship resistance for live content.** Acestream streams survived DMCA
   takedowns because there was no single origin to pull. c0mpute nodes are
   jurisdictionally diverse — a stream that legal pressure can't kill is a
   genuine product differentiation no AWS or Cloudflare account can offer.

3. **Broadcaster economics are broken.** Twitch/YouTube keep 50% of revenue and
   can demonetize or ban without appeal. A broadcaster on c0mpute pays nodes per
   GB served; the rest goes to them. CoinPay handles escrow, per-segment
   micropayments, and payout — same stack as the rest of the marketplace.

This slot in the plugin registry is complementary to transcode (which handles
VOD) and storage (which backs the DVR/replay feature). It is the one missing
piece for a complete "decentralized video platform" story.

## Detailed design

### 1. Roles

| Role | Who | What they do |
|---|---|---|
| **Broadcaster** | Content creator | Pushes RTMP/SRT, pays for ingest + relay |
| **Ingest node** | c0mpute worker | Receives stream, runs FFmpeg segmenter, seeds segment 0 of every chunk |
| **Relay/CDN node** | c0mpute worker | Fetches segments from swarm, serves to viewers and peer nodes |
| **Viewer client** | End user | Fetches m3u8 + segments; optionally seeds already-downloaded segments back to the swarm |

A single c0mpute node can play multiple roles simultaneously (ingest + relay).
The broadcaster is not required to run a node.

### 2. Ingest pipeline

```
Broadcaster
  │  RTMP or SRT (e.g. OBS, ffmpeg -f rtmp ...)
  ▼
Ingest Node (c0mpute worker, live-stream plugin)
  │  FFmpeg: segment into HLS
  │    -f hls -hls_time 4 -hls_flags delete_segments+independent_segments
  │    -hls_segment_type mpegts -hls_segment_filename 'seg_%06d.ts'
  │    -master_pl_name master.m3u8
  │    Output variants: 1080p, 720p, 480p, 360p (optional, GPU-accelerated)
  │
  ├── Announce each completed segment to DHT
  │     key: SHA256("live-stream-seg:" + stream_id + ":" + variant + ":" + seq)
  │     value: {node_id, endpoint, segment_hash, expires_at}
  │
  └── Serve segments over HTTP/3 (or HTTP/2) from local store
        GET /stream/<stream_id>/<variant>/<seq>.ts
```

Ingest nodes run FFmpeg in segmenter mode. GPU acceleration (NVENC, QSV, AMF,
VideoToolbox) is optional — CPU-only works fine for remuxing, GPU is needed
for live transcode to multiple bitrates simultaneously.

If the broadcaster's source is already HLS (e.g., an IP camera), the ingest
node can skip FFmpeg and forward segments directly.

### 3. Segment swarming (the Acestream model)

Each HLS segment (typically 2–6 seconds of video, typically 500 KB–3 MB) is
content-addressed by its SHA-256 hash. Nodes that have the segment advertise
it in the DHT; other nodes and viewer clients fetch it from whoever has it.

```
Segment lifecycle:

  seq N produced by ingest node
       │
       ├── Ingest node announces seg N in DHT
       │
       ├── Nearby relay nodes see the DHT announcement, fetch seg N from ingest
       │      (proactive push-gossip: ingest also pushes to its direct peers)
       │
       ├── Those relays re-announce seg N in DHT
       │
       └── Viewer clients request seg N from nearest node
              │  If node has it: serve from local cache
              └── If node doesn't: fetch from swarm, cache, serve, re-announce
```

Proactive push (vs. pull-on-demand): the ingest node pushes each new segment
immediately to its N closest peers (default N=5) without waiting for DHT
lookup. This primes the swarm before viewers even request the segment,
keeping latency low.

DHT records expire after `2 × segment_duration × swarm_grace_factor` (default:
30 seconds for a 4-second segment). Expired segments are evicted from relay
cache unless DVR mode is active.

### 4. HLS playlist service

Any relay node that has recent segments can serve an HLS playlist for the
stream. Viewers point their player at any relay — or at a "nearest relay"
redirect URL:

```
https://stream.c0mpute.com/<stream_id>/master.m3u8
  → 302 to nearest relay (DNS-based geo or anycast)
```

**Master playlist** (`master.m3u8`, served by any relay):

```m3u8
#EXTM3U
#EXT-X-VERSION:6
#EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080,CODECS="avc1.640028,mp4a.40.2"
1080p/stream.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720,CODECS="avc1.4d0028,mp4a.40.2"
720p/stream.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1500000,RESOLUTION=854x480
480p/stream.m3u8
```

**Variant playlist** (`1080p/stream.m3u8`, rebuilt by relay every segment):

```m3u8
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:4
#EXT-X-MEDIA-SEQUENCE:12340
#EXT-X-PROGRAM-DATE-TIME:2026-06-04T20:00:00.000Z
#EXTINF:4.000,
../seg/12340.ts
#EXTINF:4.000,
../seg/12341.ts
#EXTINF:4.000,
../seg/12342.ts
```

Segment URLs resolve to the serving relay's own cache. The relay fetches from
the swarm if it doesn't have the segment yet (transparent to the viewer's player).

#### 4a. Low-Latency HLS (LL-HLS)

For sub-3-second latency (sports scores, live reactions):
- Partial segments (0.5–1 second) announced via `EXT-X-PRELOAD-HINT`
- `EXT-X-SERVER-CONTROL: CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=0.5`
- Relay nodes support HTTP/2 server push for partial segments

LL-HLS is opt-in per stream: `c0mpute stream create --latency low`.
Standard mode targets 6–10 second latency; low-latency mode targets 2–4 seconds.

### 5. Stream descriptor and DHT discovery

When a stream starts, the ingest node publishes a stream descriptor to the DHT:

```json
{
  "v": 1,
  "stream_id": "<uuid-v4>",
  "title": "Championship Final — Live",
  "broadcaster_did": "did:coinpay:user:abc123",
  "started_at": 1748995200,
  "mode": "standard",
  "ingest_nodes": [
    "https://node1.c0mpute.com",
    "https://node2.c0mpute.com"
  ],
  "variants": [
    { "name": "1080p", "bandwidth": 6000000, "width": 1920, "height": 1080 },
    { "name": "720p",  "bandwidth": 3000000, "width": 1280, "height": 720  },
    { "name": "480p",  "bandwidth": 1500000, "width": 854,  "height": 480  }
  ],
  "dvr_window_secs": 0,
  "payment": { "model": "per_gb", "rate_usd_per_gb": 0.01 },
  "sig": "<Ed25519 sig by broadcaster_did>"
}
```

DHT key: `SHA256("live-stream-descriptor:" + stream_id)`.

Stream discovery:
```bash
c0mpute stream list              # shows active streams on the network
c0mpute stream info <stream_id>  # prints descriptor + active relay count
c0mpute stream watch <stream_id> # opens in local HLS player (mpv/vlc)
```

### 6. Payment model

Live streaming uses a **continuous per-GB settlement** model, distinct from
the one-shot transcode auction:

| Flow | Mechanism |
|---|---|
| Broadcaster funds escrow | `coinpay escrow create --stream <id> --amount <n> --token USDC` |
| Relay nodes serve bytes | Track per-segment bytes served, signed by node |
| Epoch settlement (every 60s) | Each relay submits a signed bandwidth receipt |
| CoinPay pays out | Escrow releases per receipt; relays earn immediately |
| Escrow depleted | Stream is gracefully terminated (broadcaster gets 30-second warning) |

**Ingest node** earns separately from relay nodes:
- Per-minute fee for running FFmpeg (especially if doing live transcode)
- Set as part of the stream creation auction (same offer/bid shape as transcode)

**Viewer contribution (optional)**: viewers running the `c0mpute` client can
opt in to seeding segments they've already downloaded, earning a small rebate
on bandwidth contributed. This is opt-in and clearly disclosed.

### 7. Multi-ingest (redundancy / censorship resistance)

A broadcaster can push to multiple ingest nodes simultaneously (OBS supports
multiple stream outputs). Each ingest node independently segments and seeds.
If one is taken down, others continue. Relay nodes pull from whichever ingest
is reachable.

```bash
# Broadcaster gets two push URLs
c0mpute stream create --redundancy 2
# → rtmp://node7.c0mpute.com/live/abc123?key=<stream_key>
# → rtmp://node23.c0mpute.com/live/abc123?key=<stream_key>
```

Relays deduplicate segments by hash — serving the same segment from two ingest
sources produces the same content hash (assuming deterministic FFmpeg settings),
so only one copy is stored.

### 8. DVR / replay

When `dvr_window_secs > 0`, relay nodes persist segments beyond the live window
using the storage plugin (DIP-0012). Viewers can seek back up to the DVR window:

```m3u8
#EXT-X-PLAYLIST-TYPE:EVENT
```

Full VOD archives (stream → VOD conversion after broadcast ends) are handled by
passing the persisted segment set to the transcode plugin for repackaging.

### 9. CoinPay DID integration

- **Stream ownership**: the `broadcaster_did` in the stream descriptor is
  verified on ingest. Only the owner can push to that stream key.
- **Stream key**: `coinpay stream-key create --stream <id>` generates a
  one-time-use HMAC key bound to the broadcaster's DID. Revocable.
- **Relay reputation**: relay nodes earn `stream.bytes_served` counter on their
  DID; dispute (corrupt segment) increments `stream.disputes`. High dispute rate
  causes reputation slash and removal from relay pool.
- **Broadcaster reputation**: completed streams without refund requests
  increment `stream.broadcasts_completed` — useful for building trust with
  relay node operators who might otherwise reject unknown broadcasters.

### 10. CLI surface

```bash
# Broadcaster
c0mpute stream create [--title "..."] [--variants 1080p,720p,480p] \
                      [--latency standard|low] [--dvr 3600] \
                      [--redundancy 2]
# → prints stream_id + RTMP/SRT push URLs + playback m3u8 URL

c0mpute stream start <stream_id>  # opens ingest; broadcaster pushes to URL
c0mpute stream stop  <stream_id>
c0mpute stream status <stream_id> # viewers, bitrate, relay count, escrow balance
c0mpute stream fund  <stream_id> --amount 50 --token USDC

# Viewer
c0mpute stream list               # browse live streams
c0mpute stream watch <stream_id>  # play in mpv/vlc/ffplay
c0mpute stream url   <stream_id>  # print m3u8 URL for external player

# Node operator
c0mpute stream relay --enable     # opt this node into relay role
c0mpute stream relay --disable
c0mpute stream relay --status     # bytes served, earnings, active streams

# Ingest node (usually automatic, but can be explicit)
c0mpute stream ingest --enable    # opt into ingest role (requires ffmpeg cap)
```

### 11. Web UI surface (`c0mpute.com/stream`)

- **Browse page**: grid of live streams (thumbnail from latest keyframe,
  viewer count, broadcaster DID alias)
- **Watch page**: hls.js player, chat (via secure-chat plugin, DIP-0018),
  tip button (coinpay micropayment)
- **Broadcaster dashboard**: stream health (bitrate graph, relay count,
  segment latency heatmap), escrow balance, earnings history
- **Embed**: `<iframe src="https://c0mpute.com/stream/<id>/embed">` — drops
  into any website, plays via hls.js

### 12. Protocol message types

Extends the c0mpute gossip protocol with three new message types:

| Type | Value | Purpose |
|---|---|---|
| `STREAM_DESCRIPTOR` | `0x30` | New stream announced |
| `SEGMENT_AVAILABLE` | `0x31` | Node has segment N of variant V ready |
| `SEGMENT_REQUEST`   | `0x32` | Node requests segment from peer (direct fetch fallback) |

### 13. Security considerations

| Threat | Mitigation |
|---|---|
| Rogue ingest (stream hijack) | Push URL includes HMAC stream key bound to broadcaster DID; ingest verifies before accepting |
| Corrupt segment injection | Segment hash verified by relay before serving; corrupt segment triggers relay reputation slash |
| Replay / stale segment | Segment sequence number + stream_id in DHT key; old seqs expire quickly |
| DDoS on ingest | Multi-ingest redundancy; ingest nodes rate-limit per source IP |
| DMCA / takedown | No single origin; relay nodes are pseudonymous; content hash doesn't reveal broadcaster identity |
| Escrow drain attack | Relay bandwidth receipts are size-bounded and signed; CoinPay escrow enforces max payout rate per epoch |
| Free-rider viewer | Viewer seeding is opt-in, not enforced; broadcasters fund the relay pool via escrow — no viewer gate needed |

## Alternatives considered

**Livepeer.** Livepeer does decentralized transcoding but uses its own token
and its own network. Relay/CDN is still traditional HTTP. We get transcoding
from our own `transcode` plugin and can run Livepeer as an optional
transcoding backend if desired — it's not a substitute for p2p segment delivery.

**IPFS pubsub.** Too high latency (pubsub gossip is not segment-delivery-shaped)
and IPFS has known performance problems with large binary blobs at high throughput.
Content-addressed segment hashes are borrowed from IPFS; the transport is not.

**WebRTC (peer-to-peer, sub-second latency).** Better latency but browser WebRTC
scales poorly (each viewer needs a separate connection tree). Fine for 10-person
video calls; not for 100,000 concurrent viewers. A WebRTC ingest → HLS relay
bridge (WHIP/WHEP → HLS) is a possible future addition for ultra-low-latency
ingestion.

**CDN resale (buy Cloudflare Stream wholesale).** Centralized, not censorship-
resistant, doesn't use the c0mpute network, doesn't earn relay nodes anything.
Not a product we want to build.

**SRT-only p2p.** SRT gives low-latency ingest but doesn't solve the last-mile
relay problem. HLS is the right output format because every device and browser
already supports it.

## Migration & rollout

1. **v0.1 — ingest + single-relay.** One ingest node, one relay node, standard
   HLS output. No swarm yet — relay fetches directly from ingest. Proves the
   FFmpeg → HLS → viewer path.
2. **v0.2 — DHT segment announcements + basic swarm.** Relay nodes discover
   segments via DHT and fetch from each other. Multiple relays per stream.
3. **v0.3 — payment integration.** CoinPay escrow, per-GB receipts, epoch
   settlement. Stream creation auction for ingest nodes.
4. **v0.4 — multi-ingest redundancy.** Broadcaster can push to 2+ ingest nodes.
   Relay deduplicates by hash.
5. **v0.5 — DVR + VOD handoff.** Persisted segments via storage plugin.
   Post-broadcast VOD via transcode plugin.
6. **v0.6 — LL-HLS.** Low-latency mode, partial segments.
7. **v0.7 — web UI.** Browse, watch, embed, broadcaster dashboard.
8. **v1.0 — viewer seeding.** c0mpute client optionally re-seeds downloaded
   segments, earns rebate.

## Open questions

- **Segment duration tradeoff.** 4-second segments are standard HLS; 2-second
  reduces latency but doubles segment count and DHT pressure. What's the right
  default?
- **Viewer seeding UX.** Opt-in seeding by viewers requires the c0mpute client
  to be running. How do we handle browser-only viewers (hls.js) who can't seed?
  Accept that they're free-riders, or add a WebRTC-based browser seeding path?
- **Content moderation.** Relay nodes serve content they can't decrypt or
  inspect (for live streams, content is not encrypted). What's the relay
  operator's liability? Need a clear ToS and DMCA takedown flow for relay
  operators who want legal cover.
- **Stream discovery spam.** Anyone can announce a stream in the DHT. How do we
  prevent the browse page from being flooded with garbage streams? Proposed:
  require a small escrow deposit to appear on the browse page (spam-prevention
  via economic cost). Streams with no escrow are still usable but not listed.
- **Keyframe alignment for ABR.** Adaptive bitrate switching requires keyframe
  alignment across variants. Does the FFmpeg ingest pipeline enforce this
  reliably? Needs testing.
- **Ingest failover UX.** If the primary ingest node goes down mid-stream and
  multi-ingest is active, viewers experience a brief stall. Is that acceptable,
  or do we need a seamless switchover mechanism?

## Out of scope

- WebRTC / sub-second latency (v2+)
- DRM / content protection (c0mpute is the censorship-resistant network; DRM
  is structurally opposed to that goal)
- Interactive live features beyond chat (polls, reactions — those are app-layer)
- Transcoding quality optimization (that's the transcode plugin's domain)
- Mobile ingest app (use OBS Mobile or any RTMP app)
- Audio-only streams (works with the same protocol, no special handling needed)
