# Experimental Scallion encrypted hop frames (v1)

**Status: isolated cryptographic unit-test primitive, NOT a functional
privacy network, VPN, OpenVPN server, or audited cryptographic protocol.**

`c0mpute_net::scallion_frames` implements nested ciphertext for three
configured hops and verifies that each relay can open only its own
layer. It does not connect to any network socket and has no exit path.

> **Superseded for key agreement.** The authenticated, forward-secret
> successor is the [circuit handshake](scallion-handshake.md)
> (`c0mpute_net::scallion_handshake`). This module is kept as the v1
> primitive; follow-up items 1 and 2 below are addressed there, still as an
> unwired, unreviewed state machine.

## What is actually implemented

- Exactly three hop layers in the order **guard → middle → exit**.
- The client chooses three externally authenticated and distinct
  X25519 relay public keys and distinct peer identifiers.
- Per-circuit random 128-bit ID and a fresh client X25519 ephemeral
  keypair per hop.
- Each layer derives a different AES-256-GCM key with X25519 ECDH +
  HKDF-SHA256. The HKDF context binds the circuit, role, client
  ephemeral key and relay public key.
- Binary frames have a length bound, version, circuit ID, hop index,
  sender ephemeral key, monotonic sequence and AEAD-authenticated
  header. Nonces are unique within one live client circuit because
  the sender internally increments the sequence number.
- A relay decrypts one layer into the next peer ID plus opaque
  nested frame, or the exit's plaintext. It checks the circuit, role,
  AEAD authentication and rejects duplicate/out-of-order sequence
  numbers. A failed decryption does not advance replay state.
- Malformed lengths, oversized payloads, low-order relay keys,
  duplicate relay identifiers/keys, header/ciphertext modification,
  wrong relay keys and replayed packets are tested.

```sh
cargo test -p c0mpute-net scallion_frames --locked
```

## Critical security limitations

This is **NOT Tor / a full anonymity protocol**:

- The relay X25519 key must be securely bound to the intended libp2p
  peer identity, but the key authentication and identity-binding
  handshake are **not implemented** here. Unauthenticated peer keys
  invite active man-in-the-middle attacks.
- Long-lived relay static keys provide **no forward secrecy
  against later compromise of those keys**. An authenticated
  ephemeral-ephemeral handshake with replay-protected circuit setup,
  key rotation, revocation and forward secrecy is required.
- No actual forwarding, multiplexing, flow control, padding, path
  diversity attestations, congestion control, traffic shaping,
  privacy-preserving probing or packet loss recovery is implemented.
- The strict increasing replay check only suits **one ordered stream**.
  Reordering would be rejected. A production transport needs
  independent stream keys/counters and a bounded sliding replay window.
- A guard sees the client's network IP and next hop; an exit sees the
  destination and plaintext if the application doesn't use TLS.
  Global observers can correlate timing and bytes. No anonymity claim
  is justified by layer encryption alone.
- In-memory counters must not be rolled back or resumed from a stale
  checkpoint. Every new circuit needs fresh keys and a random ID.
- A middle hop is not verified to be independent of its neighbors.
  Claimed operator IDs/ASNs are not authenticated proof.
- An opt-in, egress-restricted exit service, TUN/SOCKS5 interface,
  DNS/IPv6 fail-closed behavior, OpenVPN-compatible gateway,
  independent multi-node tests and outside security review remain
  **mandatory before user-facing connections can be enabled**.

## Follow-up implementation sequence

1. Specify and review an authenticated ephemeral relay handshake with
   forward secrecy, circuit teardown, limits and anti-replay.
2. Bind relay keys and capabilities to signed libp2p identities and
   operator consent.
3. Carry these frames over authenticated P2P streams through exactly
   three independent relay nodes, with bounded per-circuit memory,
   bandwidth, connection counts and explicit exit policy.
4. Add ordered stream multiplexing with distinct traffic keys and
   rekeying, and test relay churn/failures. Do not pretend arbitrary
   established TCP sockets survive an exit-IP change.
5. Integrate with a native OS-specific TUN client, **separately**
   support OpenVPN Connect via an opted-in OpenVPN-protocol entry
   gateway. Complete IPv4/IPv6/DNS/killswitch leak tests.
6. Run adversarial E2E tests across real independent hosts and obtain
   independent security assessment before any production anonymity
   or VPN branding.

See [Scallion architecture](scallion-network.md),
[client modes](scallion-clients.md), #386 and #389.
