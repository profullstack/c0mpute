# Hosting: static sites kept alive by the network

`hosting` is a default role. Publish a directory of static files once; hosting nodes
keep three copies of it, replace any copy whose node disappears, and every c0mpute
gateway serves it. There is no coordinator anywhere in the path.

```sh
c0mpute hosting publish ./public --name my-site            # first version
c0mpute hosting publish ./public --name my-site            # same name again = new version
c0mpute hosting publish ./public --name my-site --domain blog.example.org
c0mpute hosting ls                                         # sites a gateway knows, with copy counts
```

`publish` signs the site with this machine's identity (`<data_dir>/buyer/identity.key`),
seeds it, and stays up until three hosting nodes hold copies (default wait 10 min).
Only the machine holding that key can publish new versions of the site.

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

Limits per site: 1 GiB, 10,000,000 files. Up to 5,000 files the manifest is one chunk;
above that it is a root listing parts of 4,000 files each (by first path), so a
gateway fetches only the part a request needs. Nodes before v0.3.0 cannot read
parted manifests and skip those sites. A node holds at most `hosting.max_bytes`
(default 5 GiB) of other people's sites, and at most 1 GiB per owner key.

```toml
# ~/.config/c0mpute/config.toml
[hosting]
max_bytes = 5368709120
```

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
