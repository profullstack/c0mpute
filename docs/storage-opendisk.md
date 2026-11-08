# Storage on every node, listed on OpenDisk

Storage is a default role. Every node that runs it (that is every node unless
`--roles` leaves it out) serves two things on its gateway, `0.0.0.0:7777`:

| Path | What |
|---|---|
| `/storage/v1/objects/{hash}`, `/shards/{hash}`, `/manifests/{hash}`, `/status` | The CIP-002 storage API. Reads are open: a blake3 hash is the capability, and private-tier objects are ciphertext. Writes need an `x-coinpay-auth` envelope signed by the node's own ed25519 key or a key in `storage.trusted_writers`, so a stranger cannot fill the disk. |
| `/.well-known/opendisk.json` | The node's [OpenDisk 0.1](https://d1sks.pwa.pit.world/docs) descriptor: `key` (`ed25519:` + the node's identity key), live `capacity.free_gib` / `total_gib`, `price.per_gib_month`, and a `c0mpute` block with the peer id and storage API URL. |

## Listing on d1sks

A node lists itself; nothing central does it for the node. When the gateway announces a
public **https** origin, the node submits that origin to the OpenDisk marketplace
(`storage.d1sks_url`, default `https://d1sks.pwa.pit.world`) a few seconds after
start and again every 24 hours. d1sks then reads the descriptor itself, every hour,
and dedupes on the node's key.

```toml
# ~/.config/c0mpute/config.toml
[gateway]
bind     = "0.0.0.0:7777"
announce = "https://disk.example.net"   # TLS in front of :7777

[storage]
cap_bytes               = 500_000_000_000  # offer 500 GB, not the whole disk
price_usd_per_gib_month = 0.004            # the default
trusted_writers         = []               # extra ed25519 keys (64 hex) allowed to write
opendisk                = true             # false: no descriptor, no listing
```

A node without a public https origin (most home nodes, behind NAT) still serves its
descriptor locally and still stores shards for the network; it is simply not on the
marketplace until it has an origin d1sks can reach.

## On the node board

The capability ad carries `storage: { free_bytes, total_bytes, per_gib_month }`, and
`c0mpute.com/api/nodes` shows it per node (numbers only, re-validated by the aggregator).
