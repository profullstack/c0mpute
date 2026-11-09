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

Limits per site: 256 MiB, 10,000 files. A node holds at most `hosting.max_bytes`
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
involved.

## Finding gateways

A gateway whose operator sets `gateway.announce` (its public origin, e.g.
`http://203.0.113.7:7777`) advertises it in its capability ad, and the node board
publishes it as `gateway` on `/api/nodes`. Nothing is advertised without that setting,
and never in private mode. Clients that are not libp2p peers (the Moshpit edge, for
one) use this list to find gateways to fetch sites from.

## Not yet

Paid reservations, epoch settlement and proof-of-serve challenges (DIP-0015) are not
implemented. Hosting is free while it is in beta.
