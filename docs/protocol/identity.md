# Identity

**Protocol requirement.**

A c0mpute peer's identity is an ed25519 keypair, rendered as a DID:

```text
did:c0mpute:z6MkswFb62xmEDrqnknM3TP112AiH6A5YETp7gc2Qz4Wqkar
            └ multibase base58btc of (0xed 0x01 ‖ 32-byte ed25519 public key)
```

It is **self-certifying**: the DID *is* the public key. Verifying a
signature needs no registry, no lookup, and no network round trip. That
property is doing most of the work in this protocol — it is why an
indexer can be untrusted, why a receipt read off disk years later is still
checkable, and why there is nothing to run that could refuse you an
identity.

## Generated locally, offline, with no account

Running a c0mpute node creates a keypair on the machine. There is no
registration, no server to reach, and no product to sign up for. The key
is the same ed25519 key libp2p persists at `<config_dir>/identity.key`, so
a node has one identity across transport and protocol rather than two that
can drift apart.

## Byte-compatible with `did:key`

The encoding after `z` is exactly what `did:key` uses for ed25519 — the
multicodec prefix `0xed 0x01` followed by the raw public key, base58btc
with a multibase `z`. Any tooling that resolves `did:key:z6Mk…` resolves a
c0mpute identity by swapping the method name.

This is deliberate. The network needs a stable self-certifying
identifier; it does not need a new registry, and inventing an encoding
would have bought nothing but incompatibility.

## Identity is not payment

**This is a change from DIP-0007**, which made CoinPay DIDs the identity
layer. CoinPay remains the default settlement adapter and the
best-integrated one — see [receipts.md](receipts.md) — but it is no longer
in the path of being *on* the network.

The reason: binding network identity to a payment product means a node
cannot advertise capacity, be discovered, or verify a peer without that
product being reachable, and it makes "I want a different settlement rail"
an argument with the identity layer. Splitting them costs nothing — the
key already exists for libp2p — and buys offline identity creation, a
network that works when CoinPay does not, and settlement as a named choice
rather than an assumption.

Credentials attach to the key as separate assertions:

```text
coinpay.wallet          a linked wallet and payment reputation
coinpay.reputation      settlement history
nostr.pubkey            a linked Nostr identity
org.membership          organization membership
kyb / kyc               identity verification, where a buyer requires it
hardware.attestation    attested execution environment (validation L4)
provider.verification   a verified-provider credential
jurisdiction            a jurisdiction claim
sla                     a service-level commitment
```

A credential names its type, its issuer, and where the evidence lives:

```json
{
  "type": "coinpay.wallet",
  "issuer": "did:c0mpute:z6Mk…",
  "reference": "https://…"
}
```

**None is required to join.** A fresh key with no credentials is a valid
peer at the `community` trust tier, and a buyer who wants more decides
that for themselves — see [providers.md](providers.md#trust-tiers). What
is deliberately *not* specified yet is how a buyer verifies a credential;
that generalizes badly from one example, and there is currently one.

## Signatures

Detached ed25519, wire-encoded as multibase base58btc to match the DID:

```text
"sig": "z5p6RP1aX1ZAeffARCUXgBAJ12RBphawNMKnSQLhF4UWU9T8RXE5BUyALwFJqYvzHQP6kuYwCFAfckividdjCHL5B"
```

ed25519 is deterministic, so signing the same payload with the same key
twice gives the same bytes — which is what makes an envelope's content
hash a stable name for it rather than something that changes each time it
is re-signed.

What gets signed, and how it is domain-separated, is in
[envelopes.md](envelopes.md).

## Test vectors

Fixed seeds, for checking a second implementation. **Never use these for
anything real.**

| seed | DID |
|---|---|
| `0x11` × 32 | `did:c0mpute:z6MktULudTtAsAhRegYPiZ6631RV3viv12qd4GQF8z1xB22S` |
| `0x77` × 32 | `did:c0mpute:z6MkswFb62xmEDrqnknM3TP112AiH6A5YETp7gc2Qz4Wqkar` |

## Key rotation is unsolved

The DID is the key, so rotating it drops the peer's entire receipt
history. A signed rotation record linking the old identity to the new one
is the obvious answer and is **not designed yet** (DIP-0025, open
questions). Until it is, treat a c0mpute identity as long-lived and back
up the key file.

## Reference

`node/crates/c0mpute-envelope/src/identity.rs`.
