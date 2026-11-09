# Hosting: static sites kept alive by the network

`hosting` is a default role. Publish a directory of static files once; hosting nodes
keep three copies of it, replace any copy whose node disappears, and every c0mpute
gateway serves it. There is no coordinator anywhere in the path.

```sh
c0mpute hosting publish ./public --name my-site            # first version
c0mpute hosting publish ./public --name my-site            # same name again = new version
c0mpute hosting publish ./public --name my-site --domain blog.example.org
c0mpute hosting ls                                         # sites a gateway knows, with copy counts
c0mpute hosting get <site_id> movies/a.mp4                 # download one file, shows MB/s
c0mpute hosting rm --name my-site                          # take it down (publishes an empty version)
```

`publish` signs the site with this machine's identity (`<data_dir>/buyer/identity.key`),
seeds it, and stays up until three hosting nodes hold copies (default wait 10 min; for a
large site, 10 min without progress). Only the machine holding that key can publish new
versions of the site.

The folder is scanned in place: nothing is copied, and the publisher serves chunks
straight from the files while the network takes its copies, so publishing 50 GiB of
video does not need another 50 GiB of local disk. Hashes of unchanged files are kept in
`<data_dir>/hosting/scan-cache.json`, so running the same command again is quick. That
is also how to resume: if `publish` stops (Ctrl-C, laptop asleep), run it again. The
record already on the network is reused and holders keep every group they have.

The site is then served by every gateway at `http://<gateway>:7777/s/<site_id>/`.
Paths resolve like a static host: `/` and directories map to `index.html`, an
extensionless `/about` tries `about.html` and `about/index.html`, and a `404.html`
is used for misses.

## How it works

| Piece | What it is |
|---|---|
| Site record | `owner` (ed25519 key) + `name` -> `manifest` hash, `seq` (version), `size`, `domains`, signed by the owner. `site_id` = 20 hex of blake3(owner, name), so nobody else can publish under it. Newer `seq` wins. |
| Manifest | Every path with its size, MIME type and 1 MiB chunk hashes. Stored as a chunk itself, pinned by the record. |
| Gossip | Topic `c0mpute/hosting/v1`. Hosting nodes announce what they hold (site, manifest, seq) and their dialable addresses every minute, and re-gossip the records they hold every 5 minutes. Every node builds the same replica map from this. |
| Healing | A site with fewer than 3 live copies: each hosting node ranks the candidates (rendezvous hashing on site id + peer id). The top-ranked ones fetch a copy. A node that stops announcing stops counting after ~200 s and another node takes over. Above 5 copies the lowest-ranked holder lets go. A publisher seeding its own site is a source, not a copy. |
| Serving | Any gateway: local chunks when it holds them, otherwise fetched over libp2p from the fastest live holder (measured round-trip, round-robin among equally fast ones), failing over to the next and then to the DHT. Every chunk is checked against its hash. |

## Limits

| | |
|---|---|
| Site, paid (`--disks`, a v2 record) | 4 TiB |
| Site, free (v1 record) | 64 GiB |
| Files per site | 10,000,000 |
| One file | 128 GiB |
| Small sites other features publish (ad creatives, app images, podcast shows, git repos) | 1 GiB, built in memory |
| Node budget | `[hosting] max_bytes`; `0` (default) = what it holds plus 25% of the free space above the free-disk floor |
| Free-disk floor | a copy must leave max(10 GiB, 5% of the disk), capped at 50 GiB, free |
| Per owner, per node | 16 GiB of free sites; sites the node is a preferred (paid) host for do not count |

```toml
# ~/.config/c0mpute/config.toml
[hosting]
max_bytes = 0            # 0 = follow the disk; or a fixed number of bytes
```

The budget advertised to d1sks (`max_gib`) is the same number, so `--disks auto` only
picks disks with room for their share of the site.

Errors say which limit was hit and what to do (`--disks auto` for a free site over
64 GiB, split the folder above 4 TiB). A hosting node logs the limit that made it
refuse a copy (`over this node's hosting budget`, `disk nearly full`, `per-owner limit`).

## Large sites: striped placement

A site up to 1 GiB is replicated whole, exactly as before. Above that it is
**striped**: its chunks are bucketed by hash into groups, one group per 64 MiB of the
site up to 8,192 groups (512 GiB); a bigger site keeps 8,192 groups and makes each one
bigger (512 MiB at the 4 TiB cap), so a group bitmap on the gossip stays at most 1 KiB.
The count comes from the size in the signed record, and every group is placed on its own:

* The hosting nodes that rank highest for `(site, group)` (rendezvous hashing, the same
  answer on every node) take that group until it has three copies. No node needs the
  whole site; a 50 GiB folder spreads over the whole network.
* Hosting nodes gossip which groups they hold (a bitmap per site) and how many bytes
  they can still take (`room`). Only nodes that announce `room` are counted on for a
  group, and only when they have room for it. A group that stays short for 3 minutes
  brings in the next-ranked node, so a chosen node that cannot take it never stalls it.
* Healing, copy counting, garbage collection, proof-of-storage challenges (a chunk of a
  group the holder says it holds), budgets and the free-disk floor all work per group.
  A node holds each group as soon as it is complete, so a restart resumes group by group.
* Paid sites: preferred hosts take every group when there are at most three of them (a
  paid host holds the whole site), else the three that rank highest per group; each is
  paid for its share (`size x 3 / preferred`). A multi-TiB site therefore needs disks
  with that much room each; `--copies 5` cuts each host's share to 60% of the site.
* `c0mpute hosting ls` and `/hosting/v1/sites` show, for a striped site, `copies` = the
  fewest copies of any group, plus `groups`, `short_groups` and `missing_groups`.

Manifests: up to 5,000 files and 1 GiB the manifest is one chunk, byte for byte what
older nodes read. Above either, it is a root listing parts (by first path) of at most
4,000 files and 65,536 chunk hashes each, so every part is one chunk fetch even for a
4 TiB site; a gateway fetches only the part a request needs. Every holder of any group
keeps the root and all parts.

Groups are defined over chunk hashes only (the group of a chunk is its hash modulo the
group count), never file contents. A gateway finds the holders of any chunk from the
record alone, and the same layout works for a site whose chunks are ciphertext.

**Older nodes** refuse records over their own cap (256 MiB before v0.3.0, 1 GiB in
v0.3.0 to v0.3.2), so they never see a striped site: they neither hold nor serve it (a
gateway that old answers 404 for it), and they are never counted on for a group because
they do not announce `room`. Sites up to 1 GiB are unchanged and those nodes keep holding
and serving them.

## Streaming and Range requests

Gateways answer `Range` requests on `/s/<id>/...` and on custom domains: `206 Partial
Content` with `Content-Range`, only the chunks the range covers are fetched, and
`Accept-Ranges: bytes` is on every file, so `<video>` seeks. Single ranges of every form
work (`bytes=a-b`, `bytes=a-`, `bytes=-n`); a range past the end is `416`; several ranges
in one request get the whole file. `If-Range` and `If-None-Match` (`304`) use the file's
ETag. Large files stream with 8 chunks of read-ahead instead of being loaded whole first.
Video types: `.mp4`/`.m4v` `video/mp4`, `.webm`, `.mkv` `video/x-matroska`, `.mov`,
`.ogv`, `.m3u8`, `.vtt`, `.srt`, plus the audio types. Media is cached for an hour (the
ETag changes with a new version), HTML for a minute.

## Fast replication

Replication and gateway reads fetch many chunks at once (32 in flight for a copy, 8 of
read-ahead for a stream), each from the holder with the lowest expected wait (its measured
time per chunk times the requests already queued on it), so every holder of a group serves
part of a copy at the rate it can. A request slow for that peer is hedged to the next best
holder and the first answer wins; a failed holder is retried elsewhere and sorts last until
it answers again. Requests ride the existing libp2p connection (one yamux stream each). A
new holder of a group pulls from all of its holders and the publisher at once; holders that
finish a group become sources for it straight away.

Chunks travel on `/c0mpute/chunk-fetch/2.0.0`, which sends them as CBOR byte strings.
Version 1 sent an array of integers, about 1.9x the bytes, and is still used with peers
that do not speak 2. Chunk reads no longer run on the swarm's event loop.

c0mpute has no relay, so a node behind NAT (or a firewall that drops its random p2p
port) cannot be dialled. Every hosting node and publisher now keeps itself connected to
the hosting peers that announced a public address, so those peers can fetch the copies it
holds, or the site it is publishing, over that connection. Two nodes that both cannot be
dialled still cannot exchange chunks: give at least some hosting nodes a fixed, open p2p
port (`C0MPUTE_P2P_LISTEN=/ip4/0.0.0.0/tcp/<port>` and `C0MPUTE_P2P_EXTERNAL`).

Placement skips nodes that announced less room than a copy needs (whole sites too), so a
full node ranked first no longer leaves a site at zero copies.

`c0mpute hosting get <site> <path>` downloads one file the way a gateway does and prints
the rate (`--sequential` fetches the old way, one chunk at a time, to compare).

`--roles` without `hosting` means the node holds no copies; a gateway still serves
sites by proxying.

## Custom domains

1. Publish with `--domain blog.example.org` (repeatable; part of the signed record).
2. Add `TXT _c0mpute-hosting.blog.example.org "site=<site_id>"`.
3. Point `blog.example.org` (A/AAAA) at one or more gateway nodes on port 80/443
   (TLS in front of `:7777`), several for redundancy.

A gateway serves a Host only when both agree: the owner-signed record lists the
domain and the domain's TXT names the site, so neither a site owner nor a domain
owner can hijack the other. TXT answers are looked up over DNS-over-HTTPS and
cached for 10 minutes (2 minutes for a miss).

**Moshpit names**: `moshcode name c0mpute <name> <site_id>` points a Moshpit name at a
site. The Moshpit registry resolves it to a c0mpute edge (moshpit-proxy
`bin/moshpit-c0mpute-edge.ts`) that holds a Moshpit CA certificate for the name and
fetches `/s/<site_id>/` from any healthy gateway, so no DNS check on the gateway is
involved. The public edge is `edge.c0mpute.com`; live example: `alt.2600` is site
`ea0e1f5afde44477e71d` (`http://alt.2600`, `https://alt.2600` with the Moshpit root
installed, `https://alt.2600.pit.world` from the regular web).

## Finding gateways

A gateway whose operator sets `gateway.announce` (its public origin, e.g.
`http://203.0.113.7:7777`) advertises it in its capability ad, and the node board
publishes it as `gateway` on `/api/nodes`. Nothing is advertised without that setting,
and never in private mode. Clients that are not libp2p peers (the Moshpit edge, for
one) use this list to find gateways to fetch sites from.

## Paid hosting: the d1sks marketplace

Free copies are kept by volunteers. To pay hosting nodes to keep your site, pick them
from the [d1sks](https://d1sks.pwa.pit.world/disks?hosting=1) OpenDisk marketplace when
you publish:

```sh
c0mpute hosting disks --country DE,FI                      # what --disks auto would pick
c0mpute hosting publish ./public --name my-site --disks auto
c0mpute hosting publish ./public --name my-site --disks auto \
    --country DE,FI --max-price 0.01 --copies 3
c0mpute hosting publish ./public --name my-site \
    --disks 'country=DE&min_standing=50&max_price=0.01'
```

`--disks` is `auto` or a d1sks query (`country`, `max_price`, `min_gib`, `min_standing`;
unknown keys are refused). `--copies` is how many preferred hosts to pick (1-5, default 3).

**d1sks is discovery only.** `publish` asks it once. It keeps only disks that are c0mpute
hosting nodes (the descriptor has a `c0mpute.peer_id` and `accepts.hosting: true` or the
`hosting` role), drops any that ask more than `--max-price`, sit outside `--country` or
lack room for the site, and ranks the rest: cheapest first, then highest d1sks standing
(unrated last), then most free space, then peer id. The first `--copies` become the
site's **preferred hosts**. The agreed price is the highest ask among them, so each gets
at least what it asks. If d1sks is unreachable or nothing matches, the site publishes as a
plain free site. Placement, proofs, serving and settlement never touch d1sks.

### Record v2

The preferred hosts go into the signed site record:

| Field | |
|---|---|
| `v` | `2` when the record carries marketplace terms; plain sites stay `1`, byte for byte |
| `preferred_hosts` | 1-5 base58 peer ids |
| `price_per_gib_month` | agreed USD price, canonical decimal (`"0.004"`) |
| `payer` | hex ed25519 key that pays; absent = the owner |

A v2 record signs under its own domain (`c0mpute-hosting-v2`), so terms cannot be
stripped or added without breaking the signature. New nodes read v1 and v2. Nodes older
than this release ignore v2 records (they cannot verify them), so a v2 site is held only
by upgraded nodes.

### Placement

Still leaderless, still computed the same way on every node:

1. A preferred host that is live, asks no more than the agreed price (hosting nodes
   gossip their price) and has not lost preference replicates whether or not the site
   already has three copies.
2. The rendezvous fill runs exactly as before and does not wait for preferred hosts, so
   the site reaches three copies even if every preferred host is offline.
3. Once preferred hosts hold copies, free copies above `3 - preferred holders` are
   released one per round, lowest rank first. The total never drops below three.

Two preferred hosts among five nodes settle at three copies: both preferred plus one free.

### Proof of storage

Any node with `hosting.challenge` on asks each holder for one random chunk of the site,
straight from that peer, and checks its blake3 hash. The challenger signs a receipt
(`site, manifest, holder, chunk, ok, rtt, time`) and gossips it on `c0mpute/hosting/v1`.

- `challenge = "own"` (default): the sites this machine's publisher key owns or pays for.
- `challenge = "all"`: also a random sample of everyone else's (a gateway might); at most
  32 challenges per round.
- `challenge = "off"`.

A preferred host loses preference when at least 3 of its latest 5 receipts signed by the
site's owner or payer are failures. Receipts from anyone else are kept and shown but
cannot demote anybody. The site's copies are kept up by the normal fill meanwhile.

This proves the peer returned the right bytes for a chunk it could not predict, within the
timeout. It does not prove the peer keeps its own copy rather than relaying from another
holder.

### Pricing and settlement

The publishing machine's node is the payer. Keep a c0mpute node running there: it signs
the receipts and settles once per `hosting.settle_every_secs` (default a day) over the
window since the last round:

```text
owed = price_per_gib_month x site GiB x (window / 30 days) x passed / challenged
```

counting only receipts signed by the owner or payer. Under half passed, or never
challenged, earns nothing for that window. Amounts are integer micro-USD, rounded down.
Each transfer has an idempotency key (site, holder, window) recorded in
`<storage>/hosting/ledger.json`, so nothing is paid twice; windows never overlap.

Hosting nodes say what they ask and where to be paid:

```toml
[hosting]
price_usd_per_gib_month = 0.004     # in the OpenDisk descriptor and hosting gossip
payout_wallet = "0x..."             # where payers send money
payout_coin   = "USDC_POL"          # CoinPay currency code
challenge     = "own"               # own | all | off
challenge_every_secs = 600
settle_every_secs    = 86400
```

### Settlement switch (off by default)

With `[payments]` off, which is the default, settlement is a dry run: the node logs what it
would pay (`hosting settlement (dry run ...)`) and moves nothing. To pay for real, from the
payer's own CoinPay business wallet:

1. In CoinPay, create a business API key with the `payouts:create` scope on a business
   whose payout wallet holds the coin your hosts want (e.g. USDC on Polygon).
2. Put the key in the node's environment, never in config.toml:
   `export C0MPUTE_COINPAY_PAYOUT_KEY=...` (the variable name is
   `payments.coinpay.api_key_env`).
3. Turn payments on in `~/.config/c0mpute/config.toml`:

   ```toml
   [payments]
   enabled = true
   rail = "coinpay"
   max_usd_per_round = "10"            # hard cap per settlement round
   [payments.coinpay]
   url = "https://coinpayportal.com"
   recipient_email = "you@example.org" # CoinPay files every payout under an email
   ```

4. Restart the node. Each round then calls CoinPay `POST /api/payouts/create` once per
   preferred holder that is owed something and has a payout wallet, up to the cap.

`enabled = true` without the key is an error in the log and settlement stays a dry run.
CoinPay has no balance call for an API key, so the round cap is the spending limit.
Payments move only between the payer and the hosts it chose; c0mpute holds nothing.

## Not yet

Escrow (money held until a window is proven) and proof-of-serve challenges from DIP-0015
are not implemented: payers pay from their own wallet after the fact. Free hosting stays
free.

## Private sites

`c0mpute hosting publish ./dir --name mysite --private` seals the whole site on
this machine and prints a link like `https://gateway.c0mpute.com/p/<id>/#k=<key>`.
Gateways serve a fixed loader page and the ciphertext; the browser decrypts it
with the key after `#`, which is never sent to the gateway. Republishing keeps
the link; `--rekey` invalidates it. See [private storage](private-storage.md).
