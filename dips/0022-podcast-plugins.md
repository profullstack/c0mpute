---
dip: 0022
title: "podcasting + podcasts — p2p podcast publishing and listening on c0mpute"
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

Two companion plugins cover the full podcast lifecycle on the c0mpute network:

- **`podcasting`** (publisher) — create and manage shows, upload or live-stream
  episodes, serve censorship-resistant RSS feeds from p2p nodes, monetise via
  CoinPay subscriptions / pay-per-episode / audio ads (ads-manager) /
  value-for-value streaming payments (Podcasting 2.0).
- **`podcasts`** (consumer/listener) — discover, subscribe, download, and play
  episodes; stream live shows; support creators directly via CoinPay; works
  with any existing podcast app via standard RSS.

Both plugins speak standard RSS 2.0 with the Podcasting 2.0 namespace
(`podcast:`) and the iTunes namespace (`itunes:`), so shows hosted on c0mpute
are immediately compatible with Apple Podcasts, Spotify (via RSS), Pocket
Casts, Overcast, and every other podcast app — no new ecosystem required.

c0mpute adds what centralised podcast hosts cannot: p2p episode distribution
(no bandwidth bill), censorship resistance (no Apple/Spotify deplatforming),
live podcast streaming (DIP-0019), automatic transcription (whisper plugin),
and direct creator monetisation with zero platform cut.

## Motivation

Podcast hosting is a textbook case of unnecessary centralisation:

1. **Bandwidth cost.** A show with 50,000 downloads per episode at 50 MB/ep
   pays ~$1,500/month in bandwidth to Buzzsprout/Libsyn/Anchor. p2p distribution
   collapses that cost — listeners seed what they've already downloaded.

2. **Deplatforming risk.** Apple, Spotify, and YouTube have banned or quietly
   suppressed shows without due process. An RSS feed served from 20 c0mpute
   nodes across 10 jurisdictions is structurally immune to a single takedown.

3. **Monetisation tax.** Spotify takes ~30% of premium subscription revenue.
   Patreon takes 8–12%. Supercast takes 4%. CoinPay direct payments have no
   platform cut — only the gas-equivalent network fee.

4. **No live podcast standard.** Live podcast episodes (Twitter Spaces,
   Clubhouse, Spotify Live) are all walled gardens. Podcasting 2.0's
   `<podcast:liveItem>` is the open standard but there's no open p2p
   infrastructure to host it. c0mpute's live-stream plugin (DIP-0019) is
   exactly that infrastructure.

5. **Transcription locked behind paywalls.** Spotify and Apple charge for
   auto-transcription. c0mpute's `whisper` plugin does it on the network
   for a fraction of the cost.

## Detailed design

### 1. Show model

A podcast **show** is the top-level entity, owned by a podcaster's CoinPay DID:

```json
{
  "id": "<uuid>",
  "owner_did": "did:coinpay:user:abc123",
  "title": "The Decentralised Future",
  "description": "Weekly deep-dives into open protocols.",
  "author": "Jane Smith",
  "language": "en",
  "categories": ["Technology"],
  "artwork_cid": "<blake3_hash_of_3000x3000_jpg>",
  "explicit": false,
  "monetisation": {
    "model": "free_with_ads",
    "subscription_usd_month": null,
    "ads_enabled": true,
    "value4value": true
  },
  "feeds": {
    "rss": "https://node.c0mpute.com/podcasting/<id>/rss.xml",
    "native": "c0mpute://podcast/<id>"
  },
  "created_at": "2026-06-01T00:00:00Z"
}
```

Show records are DHT-announced (`SHA256("podcast-show:" + id)`) and replicated
to any node with the `podcasting` service role. The RSS feed is served by any
of those nodes — the URL is stable even if one node goes offline.

### 2. Episode model

```json
{
  "id": "<uuid>",
  "show_id": "<show_uuid>",
  "title": "Episode 42: Why RSS Still Rules",
  "description": "...",
  "pub_date": "2026-06-04T12:00:00Z",
  "duration_secs": 3612,
  "audio_cid": "<blake3_hash>",
  "audio_mime": "audio/mpeg",
  "audio_bytes": 90300000,
  "chapters_cid": "<blake3_hash_of_chapters_json>",
  "transcript_cid": "<blake3_hash_of_vtt>",
  "season": 2,
  "episode": 42,
  "episode_type": "full",
  "explicit": false,
  "guid": "<globally_unique_id>"
}
```

Audio files are stored via the `storage` plugin (DIP-0012, RS 10/14 erasure
coding) and served from HTTP shard endpoints on storage nodes. The audio URL
in the RSS feed resolves to a c0mpute storage node:
```
https://node.c0mpute.com/storage/<audio_cid>/episode.mp3
```

Listeners with the c0mpute client running contribute bandwidth by seeding
episodes they've already downloaded — the same BitTorrent-style swarm
as DIP-0012.

### 3. RSS feed format

The feed served at `/podcasting/<show_id>/rss.xml` is standard RSS 2.0 with
Podcasting 2.0 and iTunes namespace extensions:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"
  xmlns:podcast="https://podcastindex.org/namespace/1.0"
  xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">
  <channel>
    <title>The Decentralised Future</title>
    <link>https://c0mpute.com/podcasting/<show_id></link>
    <description>Weekly deep-dives into open protocols.</description>
    <language>en</language>
    <itunes:author>Jane Smith</itunes:author>
    <itunes:explicit>no</itunes:explicit>
    <itunes:image href="https://node.c0mpute.com/storage/<artwork_cid>/art.jpg"/>
    <itunes:category text="Technology"/>

    <!-- Podcasting 2.0: value block (streaming payments) -->
    <podcast:value type="lightning" method="keysend">
      <podcast:valueRecipient name="Jane Smith" type="node"
        address="<wallet_address>" split="95"/>
      <podcast:valueRecipient name="c0mpute protocol" type="node"
        address="<protocol_wallet>" split="5"/>
    </podcast:value>

    <!-- Podcasting 2.0: GUID for the show -->
    <podcast:guid><show_id></podcast:guid>

    <!-- Live episode (when streaming) -->
    <podcast:liveItem status="live" start="2026-06-05T20:00:00Z">
      <title>Live Q&amp;A — Episode 43</title>
      <podcast:contentLink href="https://node.c0mpute.com/stream/<stream_id>/audio.m3u8">
        Listen live
      </podcast:contentLink>
      <guid>live-<stream_id></guid>
    </podcast:liveItem>

    <item>
      <title>Episode 42: Why RSS Still Rules</title>
      <description>...</description>
      <pubDate>Wed, 04 Jun 2026 12:00:00 +0000</pubDate>
      <enclosure url="https://node.c0mpute.com/storage/<audio_cid>/episode.mp3"
        length="90300000" type="audio/mpeg"/>
      <guid isPermaLink="false"><episode_guid></guid>
      <itunes:duration>3612</itunes:duration>
      <itunes:season>2</itunes:season>
      <itunes:episode>42</itunes:episode>

      <!-- Podcasting 2.0: chapters -->
      <podcast:chapters url="https://node.c0mpute.com/storage/<chapters_cid>/chapters.json"
        type="application/json+chapters"/>

      <!-- Podcasting 2.0: transcript -->
      <podcast:transcript url="https://node.c0mpute.com/storage/<transcript_cid>/episode.vtt"
        type="text/vtt" language="en"/>
    </item>
  </channel>
</rss>
```

This feed works in any podcast app today. No migration or special client needed
for listeners who just want to subscribe via their existing app.

### 4. Live podcast streaming

Live episodes use the `live-stream` plugin (DIP-0019) in audio-only mode:

```bash
c0mpute podcasting live start --show <show_id> --title "Live Q&A"
# → starts an audio-only HLS stream
# → automatically publishes <podcast:liveItem> to the RSS feed
# → returns RTMP ingest URL for the podcaster's audio source (OBS, icecast, etc.)
```

The live stream appears in the RSS feed immediately as a `<podcast:liveItem>`.
Listeners who are subscribed get notified (if they use a Podcasting 2.0 app).
When the live episode ends:
```bash
c0mpute podcasting live end --show <show_id>
# → stops ingest
# → archives the audio to storage plugin
# → converts <podcast:liveItem> to a normal <item> with the recording
# → optionally triggers whisper plugin for transcript
```

### 5. Transcription

Episodas are automatically transcribed via the `whisper` plugin:

```bash
c0mpute podcasting transcribe --episode <episode_id> [--model base.en]
```

This submits a `whisper.transcribe` job to the network (same as any other
c0mpute job — auctioned to a worker). The resulting VTT file is stored in
the storage plugin and linked in the RSS feed via `<podcast:transcript>`.

Auto-transcription can be enabled per-show:
```
c0mpute podcasting show update --transcribe-auto true
```

### 6. Monetisation

Four monetisation models, any combination can be active simultaneously:

#### 6a. Free with ads
`ads-manager` injects audio ads into episodes (DAAST-format audio ads served
by `ctv-ads` / future `audio-ads` plugin). Standard pre-roll / mid-roll /
post-roll. Podcaster receives 70% of ad revenue via CoinPay settlement.

#### 6b. Subscription (premium tier)
```bash
c0mpute podcasting subscription set --price 5.00 --period monthly
```
Listeners with an active CoinPay subscription get:
- Early access to episodes
- Ad-free playback (SSAI skips ad breaks for subscribed listeners)
- Bonus episodes marked `itunes:episodeType="bonus"`

Subscription managed via CoinPay recurring payments. Podcaster receives 100%
minus the CoinPay network fee.

#### 6c. Pay-per-episode
```bash
c0mpute podcasting episode publish --episode <id> --price 1.99
```
Episode paywalled until listener pays. CoinPay micropayment unlocks a
time-limited signed URL for the audio file.

#### 6d. Value-for-value (Podcasting 2.0)
The `<podcast:value>` block in the RSS feed enables streaming micropayments
from Podcasting 2.0-compatible apps (Fountain, Breez, Podverse). Listeners
stream sats or USDC per minute while the episode plays. No platform required —
payments go directly from listener wallet to podcaster wallet via the addresses
declared in the value block.

### 7. Podcast discovery (consumer side)

The `podcasts` plugin maintains a DHT-based podcast index:
- Shows announce themselves on publish: `SHA256("podcast-index:" + category)`
- Listeners query by category, keyword, or DID
- Shows from the wider internet (standard RSS) can also be followed — the
  client fetches and caches the RSS feed locally

```bash
c0mpute podcasts search "decentralized tech"
c0mpute podcasts browse --category Technology
c0mpute podcasts follow <rss_url_or_show_id>    # any RSS URL works
c0mpute podcasts unfollow <show_id>
c0mpute podcasts list                            # subscribed shows
```

### 8. Playback (consumer side)

```bash
c0mpute podcasts play <episode_id>      # stream from p2p swarm
c0mpute podcasts download <episode_id>  # cache locally
c0mpute podcasts queue <episode_id>
c0mpute podcasts inbox                  # unplayed episodes from subscriptions
```

The `podcasts` plugin also launches a local HTTP server on `localhost:9090`
that serves an OPML-compatible feed list and M3U playlist — compatible with
any local media player (VLC, mpv, etc.) and any podcast app that can point
at a local RSS server.

For apps that can't use a local server, `c0mpute podcasts export --opml` outputs
an OPML file importable into any podcast app.

### 9. CoinPay DID integration

- **Podcaster DID**: `did:coinpay:user:<id>` — show ownership, revenue recipient,
  feed signing key. The RSS feed is signed with the podcaster's Ed25519 key
  (via the `<podcast:podping>` extension), so listeners can verify it hasn't
  been tampered with by a relay node.
- **Listener DID**: used for subscription management, value-for-value payment
  identity, and (optionally) cross-device sync of play position and subscriptions.
- **Reputation**: podcasters earn `podcasting.episodes_published`,
  `podcasting.listeners`, `podcasting.value_received` counters on their DID —
  useful for advertisers choosing shows for campaigns via ads-manager.

### 10. CLI surfaces

**`c0mpute podcasting`** (publisher):
```bash
c0mpute podcasting show create --title "..." --category Technology
c0mpute podcasting show update <show_id> [options]
c0mpute podcasting show list
c0mpute podcasting show delete <show_id>

c0mpute podcasting episode upload --show <id> --file ep42.mp3 --title "..."
c0mpute podcasting episode publish --episode <id> [--price N]
c0mpute podcasting episode list --show <id>
c0mpute podcasting episode delete <episode_id>

c0mpute podcasting live start --show <id> --title "..."
c0mpute podcasting live end   --show <id>
c0mpute podcasting live status

c0mpute podcasting transcribe --episode <id>
c0mpute podcasting subscription set --price N --period monthly|annual
c0mpute podcasting earnings --period 30d
c0mpute podcasting stats --show <id>   # downloads, listeners, revenue
```

**`c0mpute podcasts`** (consumer):
```bash
c0mpute podcasts search <query>
c0mpute podcasts browse --category <cat>
c0mpute podcasts follow <url_or_id>
c0mpute podcasts unfollow <id>
c0mpute podcasts list
c0mpute podcasts inbox               # new episodes
c0mpute podcasts play <episode_id>
c0mpute podcasts download <episode_id>
c0mpute podcasts queue <episode_id>
c0mpute podcasts tip <show_id> --amount 5.00   # CoinPay tip to podcaster
c0mpute podcasts subscribe <show_id>            # paid subscription
c0mpute podcasts export --opml                  # export subscriptions as OPML
```

### 11. Web UI

**`c0mpute.com/podcasting`** (publisher dashboard):
- Show manager: artwork upload, description, category, monetisation settings
- Episode uploader with transcoding progress + transcription status
- Live stream launcher (RTMP key, start/end controls)
- Stats: listener count, download graph, revenue breakdown
- RSS feed preview + validation

**`c0mpute.com/podcasts`** (listener portal):
- Browse and search the c0mpute podcast index
- Show pages with episode list, subscribe button, live indicator
- Embedded audio player (HLS.js for live, standard `<audio>` for episodes)
- Subscription management + payment history

### 12. Integration with existing plugins

| Plugin | How podcasting uses it |
|---|---|
| `storage` (DIP-0012) | Episode audio, artwork, chapters, transcripts — RS erasure coded, p2p served |
| `live-stream` (DIP-0019) | Live episode streaming (audio-only HLS); `<podcast:liveItem>` in RSS |
| `whisper` | Auto-transcription of episodes; VTT stored in storage plugin |
| `transcode` | Audio format conversion (WAV/FLAC → MP3/AAC), bitrate normalisation |
| `ads-manager` (DIP-0021) | Audio ad injection (pre/mid/post roll), revenue settlement |
| `coinpay` (DIP-0007) | Subscription payments, pay-per-episode, tips, value-for-value addresses |
| `secure-chat` (DIP-0018) | Listener ↔ podcaster DMs (premium subscriber perk) |

## Alternatives considered

**Build on IPFS for episode storage.** IPFS has reliability issues with large
audio files at scale; pinning services re-centralise it. c0mpute's storage
plugin (RS erasure coding + proof-of-serve slashing) has stronger durability
guarantees and is already in the ecosystem.

**Federated ActivityPub podcast directory (like Castopod).** ActivityPub
federation is server-dependent — every instance is a central point of failure
for its subscribers. RSS is the correct protocol for podcast syndication;
c0mpute's DHT provides the serverless index layer on top.

**Single monolithic `podcast` plugin.** Publisher and consumer have very
different capability requirements: publishers need storage write access,
transcoding, RTMP ingest; consumers need local playback, OPML export, offline
caching. Splitting them keeps each lean and lets listener-only nodes avoid the
publisher infrastructure footprint.

**Spotify for Podcasters / Anchor integration.** Defeats the purpose. These
platforms own the relationship between podcaster and listener and extract rent
from both.

## Migration & rollout

1. **v0.1 — static episode hosting.** Upload audio → storage plugin →
   RSS feed served by c0mpute node. Compatible with all podcast apps.
   Manual transcription (upload VTT file). No monetisation yet.
2. **v0.2 — discovery + consumer CLI.** DHT podcast index. `c0mpute podcasts`
   search/follow/inbox/download commands. OPML import/export.
3. **v0.3 — live podcast.** `c0mpute podcasting live start/end`. Audio-only
   live-stream integration. `<podcast:liveItem>` in RSS feed.
4. **v0.4 — auto-transcription.** `whisper` plugin integration. Per-show
   auto-transcribe setting.
5. **v0.5 — monetisation.** CoinPay subscriptions, pay-per-episode, tips.
   `<podcast:value>` block for value-for-value. ads-manager audio ad injection.
6. **v0.6 — web UI.** Publisher dashboard + listener portal.
7. **v1.0 — Podcast Index registration.** Submit c0mpute-hosted shows to
   podcastindex.org so they appear in all Podcasting 2.0 apps automatically.

## Open questions

- **Feed node redundancy UX.** If a listener's subscribed feed node goes
  offline, how does the client transparently failover to another node serving
  the same show? Proposal: DHT lookup on `show_id` returns multiple nodes;
  client tries in order. Need to specify the failure detection latency target.
- **Play position sync.** Listeners expect to resume where they left off
  across devices. Where is play position stored? Options: (a) encrypted in
  storage plugin keyed to listener DID, (b) local only (simpler, no sync).
- **Podping support.** Podping is the Podcasting 2.0 standard for instant
  feed update notifications (replaces polling). Should c0mpute nodes act as
  Podping hubs, or just emit Podping events when a show is updated?
- **Minimum storage commitment.** A podcaster publishing one episode per week
  at 50 MB/ep accumulates ~2.5 GB/year. Storage plugin nodes need to commit
  to holding this for the show's lifetime. How is this contracted and priced?

## Out of scope

- Video podcasts / vodcasts (use live-stream plugin directly)
- Podcast editing / production tools
- Guest booking / scheduling
- Community features (comments, ratings) — these are app-layer concerns
- Cross-promotion / ad marketplaces beyond ads-manager
