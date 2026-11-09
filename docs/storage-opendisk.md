# Storage on every node, listed on OpenDisk

Every node is a disk provider by default: `storage` and `hosting` are on even when
`roles` in config.toml (or `--roles`) lists other roles, until the operator turns them
off (below). A node with the gateway role serves two things on `0.0.0.0:7777`:

| Path | What |
|---|---|
| `/storage/v1/objects/{hash}`, `/shards/{hash}`, `/manifests/{hash}`, `/status` | The CIP-002 storage API. Reads are open: a blake3 hash is the capability, and private-tier objects are ciphertext. Writes need an `x-coinpay-auth` envelope signed by the node's own ed25519 key or a key in `storage.trusted_writers`, so a stranger cannot fill the disk. |
| `/.well-known/opendisk.json` | The node's [OpenDisk 0.1](https://d1sks.pwa.pit.world/docs) descriptor: `key` (`ed25519:` + the node's identity key), live `capacity.free_gib` / `total_gib`, `price.per_gib_month`, and a `c0mpute` block with the peer id and storage API URL. |

A node with the `hosting` role also says so: `accepts.hosting: true`, `c0mpute.roles`, and
`c0mpute.hosting { price_per_gib_month, max_gib }`. d1sks lists those at `?hosting=1`, and
`c0mpute hosting publish --disks auto` picks paid hosts from them (see [hosting](hosting.md)).

## Listing on d1sks

A node lists itself; nothing central does it for the node. Every storage node, NAT'd or
not, pushes a **signed listing** to the OpenDisk marketplace (`storage.d1sks_url`,
default `https://d1sks.pwa.pit.world`) 20 seconds after start and then every 5 minutes:

```
POST /api/v1/disks/announce
{ "payload": "<JSON text>", "signature": "<hex ed25519 signature over the text>" }
```

The payload is `{ type: "c0mpute.disk.announce/1", marketplace, peer_id, key, ts,
expires_in, origin, descriptor }`. It is signed with the node's libp2p identity, so d1sks
checks that `key` is the key inside `peer_id` and that the signature holds; no origin
fetch is needed. `marketplace` (the d1sks host) and `ts` stop replays. The descriptor is the
same OpenDisk document the gateway serves, plus `c0mpute.addrs` (up to 8 multiaddrs, public
first) and `c0mpute.reachable`:

| `reachable` | Meaning |
|---|---|
| `https` | the gateway announces a public https origin (`gateway.announce`); d1sks shows its URL |
| `direct` | a public IP multiaddr; dialable over libp2p |
| `c0mpute` | behind NAT; reached through c0mpute over connections the node opens. Still a full holder: storage and hosting move over libp2p |

A listing expires 20 minutes after the last heartbeat, so a node that goes quiet drops off
by itself. `c0mpute hosting publish --disks auto` ranks disks seen live on the hosting
gossip first, then fresh publicly reachable ones, then fresh NAT'd ones, then stale ones
(price, standing and free space break ties); `c0mpute hosting disks --live` shows the same.

Origin-based listings still work for any OpenDisk server: `POST /api/v1/disks {"url": ...}`
and d1sks fetches `/.well-known/opendisk.json` hourly.

```toml
# ~/.config/c0mpute/config.toml
[gateway]
bind     = "0.0.0.0:7777"
announce = "https://disk.example.net"   # optional: TLS in front of :7777

[storage]
enabled                 = true             # false: not a disk provider at all
list_on_d1sks           = true             # false: store for the network, stay off d1sks
country                 = "DE"             # optional, shown and filterable on d1sks
cap_bytes               = 500_000_000_000  # offer 500 GB, not the whole disk
price_usd_per_gib_month = 0.004            # the default
trusted_writers         = []               # extra ed25519 keys (64 hex) allowed to write
opendisk                = true             # false: no descriptor, no listing

[hosting]
enabled = true                             # false: hold no sites for the network
```

| Opt-out | Config | CLI | Environment |
|---|---|---|---|
| no storage, not listed | `[storage] enabled = false` | `worker start --no-storage` | `C0MPUTE_STORAGE=0` |
| no hosting | `[hosting] enabled = false` | `worker start --no-hosting` | `C0MPUTE_HOSTING=0` |
| store, but stay off d1sks | `[storage] list_on_d1sks = false` | | `C0MPUTE_D1SKS=0` |

Private mode never lists and never adds roles the node did not ask for.

## On the node board

The capability ad carries `storage: { free_bytes, total_bytes, per_gib_month }`, and
`c0mpute.com/api/nodes` shows it per node (numbers only, re-validated by the aggregator).
