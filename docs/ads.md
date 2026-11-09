# Ads on c0mpute (v1)

Three plugins, one peer-to-peer system, no ad server and no registry:

- **ads-manager**: signed campaigns gossiped to every gateway, creatives on
  hosting, deterministic ad selection, signed impression receipts.
- **podcast-ads**: dynamic ad insertion into c0mpute-hosted podcasts that opt in.
- **ctv-ads**: VAST 4.2 and VMAP for connected TV and web video players.

Code: `node/crates/c0mpute-core/src/ads/`.

**Not built yet: billing.** `--max-cpm` only ranks campaigns. Nothing is
escrowed, charged or paid out, and there is no OpenRTB auction.

## Advertisers

```sh
# Creatives: files or a folder (an ad-generator-agent output folder works;
# an optional ad-set.json there can supply {"landing_url": "..."}).
c0mpute ads campaign create --name spring-launch \
  --creative ./adset --placement pre,post,ctv_preroll \
  --max-cpm 2.50 --landing https://example.org/ \
  --category technology --geo US,CA --freq-cap 3/day \
  --start 2026-12-01 --end 2027-01-01

c0mpute ads campaign ls                      # yours
c0mpute ads campaign ls --gateway http://gw:7777   # what a gateway knows
c0mpute ads campaign update spring-launch --max-cpm 3
c0mpute ads campaign pause spring-launch     # resume brings it back
c0mpute ads report spring-launch             # count receipts
```

`create` copies the creatives into a hosting site you own (`ads-<name>`),
publishes it, and stays up until three hosting nodes hold copies. Then it
gossips the signed record. After that your machine can go offline: gateways
fetch the creatives from the hosting copies and check every byte against the
hash you signed.

A campaign record holds your ed25519 key, `campaign_id = hash(owner, name)`,
a `seq` (higher wins, so only a newer signed version changes anything), the
creatives by hash with kind/MIME/size/duration, targeting (placements,
categories, keywords, countries), schedule, frequency cap, max CPM and
landing URL. Placements are `podcast_pre`, `podcast_mid`, `podcast_post`,
`ctv_preroll`, `ctv_midroll`, `ctv_postroll` and `display`. Podcast placements
need an audio creative, CTV a video, display an image or GIF.

## How a gateway picks an ad

The same rule runs on every gateway (`ads/select.rs`):

1. Filters, each with a named reason: paused, not started, ended, wrong
   placement, no fitting creative, category, keyword, country (or country
   unknown when the campaign targets countries), below the publisher's floor,
   frequency cap reached for this listener.
2. The highest max CPM wins.
3. Equal bids are settled by rendezvous hashing of (campaign id, slot), so
   every gateway gives the same answer for the same slot.

`GET /ads/v1/select?placement=...` (or `c0mpute ads select`) returns the
winner and why every other campaign lost.

Country comes from `?country=` or a CDN header (`CF-IPCountry`,
`X-Country`); gateways carry no GeoIP database. The listener id is a hash of
`?uid=` or of IP + user agent. Frequency caps are enforced per gateway; there
is no network-wide count.

## Receipts and reports

Each served ad event (podcast impression, VAST impression, quartiles,
complete, click) becomes a receipt. Every flush, a gateway signs one batch
per campaign. Batches number up from 0, and each names the hash of the one
before it, so a gateway cannot quietly rewrite its history and readers can
see gaps. Batches go in a JSONL log on the gateway, at
`/ads/v1/receipts?campaign=<id>`, and on gossip.

`c0mpute ads report` asks over gossip. Each gateway re-gossips its own chain
for that campaign, and the CLI verifies signatures and chains, dedupes events
by play, and counts impressions, events, placements, gateways, shows and
unique listeners. `--gateway <url>` also fetches the log straight from a
gateway. A gateway that is offline shows up as missing until it returns.

A receipt is a gateway's signed claim, not proof that someone heard or saw
the ad. Settlement will be built on these chains.

## Podcasters: opting in

No show ever gets ads unless it asks:

```sh
c0mpute podcasting publish ./show --name my-show --ads pre,post
```

You can do the same with an `[ads]` table in `show.toml`:

```toml
[ads]
placements = ["pre", "mid"]   # default pre + post
categories = ["technology"]   # default: the show's category
min_cpm = "1.50"
mid_at_secs = 600             # default: half way
max_ad_secs = 60
```

This writes `c0mpute-ads.json` into the show's site. The site record is signed
with your key and pins that file by hash, so the opt-in is your signature and
no gateway can forge one. Feed enclosures then point at
`episodes/<file>?ads=1`. The bare path stays the clean file.

On `?ads=1`, a gateway with `ffmpeg` and `ffprobe` picks one campaign per
break and fetches the audio creatives from hosting. It splices them in by
resampling to the episode's rate and layout, joining with the concat filter
and re-encoding in the episode's own format. It serves the result with a
correct `Content-Length` and Range support, and reuses it for the same
listener for 10 minutes, so range requests are not counted twice. It records
one impression receipt per ad. A missing ffmpeg, no matching campaign or any
failure gets the clean episode. In every case the `x-c0mpute-ads` response
header says what happened.

## Video players (VAST / VMAP)

```
GET /ads/v1/vast?placement=ctv_preroll&uid=<device>&categories=..&country=..
GET /ads/v1/vmap?breaks=start,600,end&uid=<device>
```

These are also served at `/ads/vast` and `/ads/vmap`, with CORS open. A VAST
reply is a 4.2 InLine ad. The `MediaFile` is served by the gateway from the
hosting swarm (`/ads/v1/creative/<campaign>/<hash>`, with Range support).
Impression, quartile and click URLs point back at the same gateway, which
HMACs them so beacons count only for ads it served, and once per play. No
fill is the empty `<VAST version="4.2"/>`.

## Gateway config

Every node that runs hosting (the `hosting` or `gateway` role) keeps the
campaign index and relays `c0mpute/ads/v1`. Only gateways answer ad requests.

```toml
[ads]
serve = true              # answer ad requests (false: empty VAST, clean episodes)
podcast_insertion = true  # splice ads into opted-in episodes (needs ffmpeg)
```

Set `gateway.announce` to the gateway's public https origin so VAST URLs are
absolute and correct behind a proxy. Without it they come from the request's
`Host`.
