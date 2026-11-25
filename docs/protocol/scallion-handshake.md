# Scallion circuit handshake (experimental, v1)

**Status:** an in-process state machine with unit tests. It opens no socket,
is not wired into the node, carries no user traffic and has had no independent
review. **Not a working VPN.** `c0mpute scallion doctor` still reports both
client modes as not ready.

Code: `c0mpute_net::scallion_handshake`.
Threat model: [scallion-threat-model.md](scallion-threat-model.md) v0.1.

It supersedes the key agreement in [frames v1](scallion-frames.md), which used
long-lived relay keys with no authentication and no forward secrecy. The
frames module is kept as the earlier primitive.

## Relay descriptors

A relay that wants to carry Scallion traffic publishes a signed descriptor.

```
body   = "SC1D" | version(1)=1
       | peer_len(1) | peer_id
       | noise_static(32)            X25519 public key
       | roles(1)                    bit0 guard, bit1 middle, bit2 exit
       | n_ports(1) | port(2)*       TCP ports the exit allows
       | op_len(1) | operator        self-declared label
       | issued_at_ms(8) | expires_at_ms(8)

signed = "SC1S" | len(2) public_key | len(2) body | len(2) signature
```

`public_key` is the relay's libp2p public key (protobuf). The signature is by
that key over `"c0mpute/scallion/relay-descriptor/v1\0" | body`. All integers
are big-endian.

A client accepts a descriptor only if:

- the public key hashes to the `peer_id` in the body, and the signature
  verifies;
- `issued_at` is not more than 5 minutes ahead, `expires_at` is in the future,
  and the lifetime is at most 7 days;
- at least one role is set; `exit` comes with at least one port, and ports are
  absent without it;
- the Noise key is not a low-order point.

Verification returns a `VerifiedRelay`. Circuits can only be built from that
type, so an unauthenticated key cannot reach the handshake.

A valid descriptor proves that the peer's identity vouches for the key and the
roles. It does not prove the operator label, that two relays have different
owners, or that the relay is honest.

## Per-hop handshake

`Noise_NK_25519_ChaChaPoly_SHA256`, using the `snow` crate (the Noise
implementation libp2p already uses). No negotiation: one pattern, one version.

- **NK**: the client knows the relay's static key and sends none of its own.
  The relay is authenticated; the client is anonymous to it.
- Message 1 is `e, es`; message 2 is `e, ee`. Both are 48 bytes. Traffic keys
  depend on `ee`, so a relay static key stolen later does not reveal them.
- **Prologue**: `"c0mpute/scallion/handshake/v1" | peer_len | relay_peer_id |
  link_id`. Both sides must agree on which relay and which link the handshake
  is for, or it fails.

After the handshake each side holds one key per direction. Cells carry an
explicit 64-bit sequence number used as the nonce.

## Link messages

Between two adjacent peers of a circuit:

```
"SC1L" | type(1) | link_id(16) | body

1 Create   handshake(48)            toward the relay being added
2 Created  handshake(48)            back toward the client
3 Cell     seq(8) | len(4) | ct     one encrypted layer
4 Destroy                           tear down on this link
```

`link_id` is random and chosen by the client for each link, so the three links
of a circuit carry three unrelated ids.

## Layers

The plaintext of every layer starts with a command byte.

Client to hop:

| Command | Body | Meaning |
|---|---|---|
| `0x01` Forward | `seq(8) \| ciphertext` | pass the inner cell to the next hop |
| `0x02` Extend | `link_id(16) \| peer_len \| peer_id \| handshake(48)` | send a `Create` to that peer |
| `0x03` Data | bytes | payload for this hop |
| `0x04` Destroy | none | tear down |

Hop to client:

| Command | Body | Meaning |
|---|---|---|
| `0x11` Relayed | `seq(8) \| ciphertext` | a cell from the next hop |
| `0x12` Extended | `handshake(48)` | the next hop's `Created` |
| `0x13` Data | bytes | payload from this hop |
| `0x14` ExtendFailed | none | the next hop could not be reached |

To reach hop *k* the client encrypts for hop *k*, then wraps one Forward layer
per hop between it and itself. A cell travelling back is encrypted by the hop
that originated it, and each hop nearer the client adds a Relayed layer.

## Building a circuit

1. Client sends `Create` to the guard; the guard replies `Created`.
2. Client sends the guard an `Extend` naming the middle. The guard sends
   `Create` to the middle and returns the reply as `Extended`.
3. Client sends the middle (through the guard) an `Extend` naming the exit.

The guard learns the middle and never the exit. The exit learns the middle and
never the guard or the client. The client checks, before extending:

- the first hop holds the guard role, the second the middle role, the third
  the exit role;
- no peer id and no Noise key appears twice;
- there are exactly three hops.

A relay refuses a second `Extend`, an `Extend` to itself or back to the peer
the circuit came from, and any message "from the next hop" that does not come
from that peer on that link.

## Replay and ordering

Receivers keep a 64-wide sliding window per hop and direction: each sequence
number is accepted once, in any order, within 64 of the newest. A cell that
fails authentication does not advance the window, so an attacker cannot burn
numbers. Senders never reuse a number and refuse to continue at exhaustion.

## Limits enforced

Payload at most 16 KiB per cell; handshake messages exactly 48 bytes; peer ids
at most 128 alphanumeric bytes; descriptors at most 1 KiB; at most 64 exit
ports. Oversized or truncated input is rejected before allocation.

## Relay limits

`c0mpute_net::scallion_relay::RelayTable` holds every circuit one relay
carries and enforces what a relay needs before it can face the network. Like
the handshake it is a state machine: the caller passes each message with the
authenticated peer id it arrived from and the current time.

| Limit | Default | Effect |
|---|---|---|
| circuits in total | 1024 | further `Create`s are refused |
| circuits per previous peer | 16 | one peer cannot fill the table |
| `Create`s per peer | 20 per 10 s | a refused `Create` costs no handshake |
| idle timeout | 120 s | the circuit is destroyed toward both sides |
| extend timeout | 15 s | the client gets `ExtendFailed`; the circuit stays |

The defaults are starting points, not measured values. A link id must be
unique per peer in both directions; an `Extend` that would reuse one fails. A
cell that fails authentication does not refresh the idle timer, so garbage
cannot keep a circuit open. When a peer disconnects, every circuit through it
is torn down and the other side is told.

## What this does not do

- No transport. Nothing here sends a byte; the caller moves messages.
- No streams, flow control, padding, rekeying or cover traffic.
- No exit. `Data` is opaque; connecting it to
  [`scallion_exit`](scallion-exit-policy.md) and a stream protocol is the next
  slice.
- No descriptor distribution, revocation or relay key persistence.
- No bandwidth or byte quotas per circuit; only counts, rates and timeouts.
- No review. The weaknesses we already know are listed in the threat model.

## Tests

```sh
cargo test -p c0mpute-net scallion_handshake --locked
```

A simulated three-relay network checks: a full build and data both ways; that
the guard never sees the exit's id or the payload and the exit never sees the
guard's; distinct link ids; descriptor round-trip, tampering, expiry, wrong
signer and bad content; a relay holding the wrong key; a handshake aimed at a
different peer id; replayed, tampered and reordered cells; path rules; extend
refusals; extend failure and retry; teardown from both ends; key separation
between circuits; and malformed link messages.

## Next

1. Carry `LinkMsg` over authenticated libp2p streams between real nodes, with
   per-peer circuit limits and timeouts.
2. A stream layer over `Data` (begin, data, end, flow control) and an exit
   that enforces `ExitPolicy`.
3. A loopback SOCKS5 client with remote DNS and no direct fallback.
4. Multi-node tests across independent hosts, then outside review.

Tracks #386.
