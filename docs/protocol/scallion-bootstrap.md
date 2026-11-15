# Scallion Network: independent bootstrap (transport prerequisite)

**Status:** experimental discovery hardening, not anonymizing transport.

A libp2p peer's local Ed25519 identity is sufficient to join the network.
Normal startup no longer needs to issue an HTTPS request to
`https://c0mpute.com/bootstrap.json`. The binary still has public
seed contacts compiled in, which means those seed operators can see
connections. To avoid those contacts, operators can supply independent
peers and opt into **community mode**.

## Modes

```sh
# Default: public libp2p seeds; no HTTP seed-directory check-in
c0mpute worker start

# Dial independently operated peers only; do not dial Profullstack seeds
export C0MPUTE_BOOTSTRAP_MODE=community
export C0MPUTE_BOOTSTRAP_PEERS='/dns4/seed1.example.net/tcp/4001/p2p/12D3KooW...'
c0mpute worker start

# Alternatively, store one full multiaddr per line, optional # comments
export C0MPUTE_BOOTSTRAP_MODE=community
export C0MPUTE_BOOTSTRAP_FILE="$HOME/.config/c0mpute/scallion-peers.txt"
c0mpute worker start

# LAN-only discovery, if mDNS is enabled; no configured WAN peers
C0MPUTE_BOOTSTRAP_MODE=offline c0mpute worker start

# Explicitly opt in to a directory (publishes your address to that host)
C0MPUTE_BOOTSTRAP_URL=https://seeds.example.net/bootstrap.json c0mpute worker start

# Restore the historical default c0mpute.com HTTPS fetch (optional)
C0MPUTE_BOOTSTRAP_FETCH=1 c0mpute worker start
```

The example `seed1.example.net` and abbreviated peer ID are
placeholders, **not functioning public seed addresses**. A real
multiaddr requires a complete peer ID.

### Variable reference

| Environment variable | Purpose |
|---|---|
| `C0MPUTE_BOOTSTRAP_MODE` | `public` (default), `community`, `offline` |
| `C0MPUTE_BOOTSTRAP_PEERS` | Comma/newline-delimited full `/p2p/<peer-id>` addresses |
| `C0MPUTE_BOOTSTRAP_FILE` | File containing full multiaddrs, one per line |
| `C0MPUTE_BOOTSTRAP_URL` | Opt-in HTTPS feed; operator-selected |
| `C0MPUTE_BOOTSTRAP_FETCH` | `1` explicitly enables legacy default feed in public mode |

The offline setting only suppresses configured bootstrap contacts;
mDNS, libp2p listen sockets, network discovery through existing
connections, telemetry from other subsystems, and application API
traffic are **separate** concerns. A truly disconnected process
requires additional network isolation.

## Security and privacy limitations

- A direct libp2p seed sees the source IP. Community bootstrap avoids
  *our* seeds, but does not anonymize the connection to independent ones.
- A user-supplied feed sees the HTTP caller IP and can recommend
  malicious or unavailable peers. It is never a trusted authority.
- A peer ID is cryptographically authenticated by the libp2p Noise
  handshake, not verified as an independent operator. Ownership, ASN,
  Sybil resistance and trust policy must be assessed separately.
- `C0MPUTE_BOOTSTRAP_MODE=community` without viable peers may be
  unable to join the public internet overlay. It must not silently
  fall back to built-in seeds.
- The Scallion Network route planner and this bootstrap change do not
  establish layered-encrypted circuits, a proxy, or a VPN. No claim
  of location anonymity is appropriate.

For the transport roadmap and session continuity see
[Scallion Network](scallion-network.md).

## Tests

`cargo test -p c0mpute-net bootstrap` tests:
- default has no HTTPS directory check-in,
- community mode excludes built-in contacts,
- offline mode skips all configured contact and feed sources,
- only complete authenticated peer multiaddrs are accepted,
- HTTPS is required for optional directory feeds,
- feed protocol versions and advertised peer IDs are checked.

Other network role integration tests must also pass before merging.
