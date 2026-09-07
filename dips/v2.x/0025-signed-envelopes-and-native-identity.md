---
dip: 0025
title: "Give every protocol message its own signature and every peer its own DID"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-09-07
updated: 2026-09-07
discussion:
implementation: node/crates/c0mpute-envelope
supersedes:
superseded-by:
---

## Summary

Four things change:

1. **Native network identity.** Every peer has a locally generated
   `did:c0mpute:z6Mk…` derived from the ed25519 key libp2p already
   persists. Identity is established offline, with no account anywhere and
   no payment product involved.
2. **Signed envelopes.** Every protocol message travels in an envelope
   carrying its own detached signature over canonical bytes, with domain
   separation by message type. Verification needs no registry lookup and
   no network round trip.
3. **Canonical serialization.** RFC 8785 with floats and non-ASCII object
   keys forbidden, so two implementations in two languages produce
   byte-identical output — and therefore identical hashes and signatures.
4. **Four payload types**: `provider.advert/v1`, `job/v2`, `offer/v1`,
   `receipt/v1` (plus `receipt.acceptance/v1` for the buyer's
   countersignature).

Implemented in `node/crates/c0mpute-envelope`, with pinned cross-
implementation test vectors in that crate's `tests/vectors.rs`.

## Motivation

### Transport signatures do not survive the first hop

The v1 market types in `c0mpute-net::topics` carry no signature. Their
comment says so plainly: *"Signed by the worker's CoinPay DID once that
lands; today we rely on gossipsub message authenticity."* That is fine
while a message is on the wire and useless everywhere else:

- an advert relayed by an indexer arrives with no provenance;
- a receipt read back from local disk after a restart cannot be checked;
- an offer forwarded by a gateway is the gateway's word;
- a reputation score computed from any of the above has to be *believed*,
  which is exactly the property that forces a trusted database into the
  middle of the network.

Signed receipts are the difference between reputation being portable and
reputation being someone's table.

### Identity should not require a payment product

DIP-0007 makes CoinPay DIDs the identity layer. CoinPay is the right
default and the best-integrated option, and it should stay that way. But
binding *network identity* to it means a node cannot advertise capacity,
be discovered, or verify a peer without a second product's availability —
and it means any organization that wants a different settlement rail has
to argue with the identity layer to get one.

Splitting them costs almost nothing (the key already exists, for libp2p)
and buys: offline identity creation, a working network when CoinPay is
unreachable, and settlement as a named adapter rather than an assumption.

### Floats and timestamps quietly fork a network

Two nodes that serialize the same logical message differently compute
different hashes and cannot verify each other's signatures. The two
reliable ways to get there are floating-point numbers (`0.1 + 0.2`, and
every encoder's disagreement about how to print a double) and timestamps
with more than one legal spelling. Both are avoidable by construction, and
neither is recoverable once signed data is in the wild.

## Detailed design

Full specification in `docs/protocol/`. The load-bearing decisions:

### Identity: `did:c0mpute:z…`, byte-compatible with `did:key`

```text
did:c0mpute:z6MkswFb62xmEDrqnknM3TP112AiH6A5YETp7gc2Qz4Wqkar
            └ multibase base58btc of (0xed 0x01 ‖ 32-byte ed25519 public key)
```

The encoding after `z` is identical to `did:key` for ed25519, so existing
DID tooling resolves a c0mpute identity by swapping the method name. Same
key as the libp2p peer id, so a node has one identity across transport and
protocol rather than two that can disagree.

Credentials — CoinPay wallet, KYB, hardware attestation, organization
membership, SLA — attach to this key as separate assertions. None is
required to join.

### Envelope

```json
{
  "v": 1,
  "type": "c0mpute.provider.advert/v1",
  "signer": "did:c0mpute:z6Mk…",
  "payload": { },
  "sig": "z5p6…"
}
```

The signature covers `"c0mpute-envelope/v1\n" ‖ type ‖ "\n" ‖
canonical_json({v, type, signer, payload})`. Domain separation by tag and
by type means a signature harvested from one message type cannot be
replayed as another — including between two types with identical field
shapes, which is a test in the crate.

`Envelope::open()` checks version, type, signature and payload structure
before returning the payload. Reading `.payload` directly is the mistake
this design exists to prevent, and everything downstream should go through
`open()` so that "we received this" and "this peer signed this" stay the
same statement.

### Canonical JSON: RFC 8785, minus its two hardest corners

- **No floats.** Money is a decimal string (`"0.042"`); durations and
  sizes are integers bounded to the safe-integer range so a browser client
  and a Rust node agree. Canonicalization *rejects* a float rather than
  guessing, so the failure is at signing time, not on someone else's
  verification.
- **ASCII object keys.** JCS sorts by UTF-16 code unit, which diverges
  from UTF-8 byte order above U+FFFF; restricting keys makes the two
  orderings identical. Values remain full UTF-8.

Both are checked, not assumed.

### Content addressing and BLAKE3

Job ids and record references are content hashes, written `blake3:<hex>`
and resolvable as `c0://blake3:<hex>`. The v2 direction's illustrative
JSON writes `sha256:`; the repository already content-addresses chunks
with BLAKE3 (`c0mpute-proto::Hash`, `c0mpute://blake3:`), and running two
hash trees over the same bytes would be a lasting tax for no gain. The
algorithm is on the wire, so this is a default, not a lock-in.

A job's id is the hash of its whole canonical manifest. The v2 direction
sketches an `id` field *inside* the manifest, which obliges every
implementation to agree on which fields to strip before hashing; deriving
it from the whole document deletes that step and its bugs.

### Two signatures, two envelopes

A receipt needs the provider's signature and the buyer's. Rather than a
multi-signature blob, the provider seals a `receipt/v1` and the buyer
seals a `receipt.acceptance/v1` naming it by hash. The signatures are made
at different times by parties who may never be online together, one
arriving without the other is normal rather than malformed, and a buyer
can say *no* — which a countersignature field cannot express.

### Freshness is a read-time check

Adverts and offers carry expiry; adverts are capped at 15 minutes.
`open()` verifies authenticity, `open_fresh(now)` also enforces expiry.
The split is deliberate: an expired advert is history, not a forgery, and
a stale cache should stop looking like live capacity without stale records
becoming unverifiable.

## Alternatives considered

**Keep leaning on gossipsub signatures.** Zero work, and it forecloses
portable reputation, offline verification, and any non-gossip transport.
The v1 comment already anticipated replacing this.

**Use CoinPay DIDs as network identity (status quo, DIP-0007).** Fewer
moving parts and a genuine integration advantage. Rejected because it puts
a payment product in the path of peer discovery and makes settlement
choice an identity-layer argument. CoinPay remains the default adapter and
the first-party integration; it is no longer load-bearing for the network
to run.

**Protobuf or CBOR instead of canonical JSON.** Both have canonical forms
and both are more compact. JSON wins here because agents, browsers and
`curl` are first-class clients (v2 direction §23), a signed message should
be readable by the person debugging it, and the deterministic-encoding
problem is solved either way. Revisit if message volume ever makes size
the binding constraint.

**Full RFC 8785 including float canonicalization.** Correct and portable
in principle; in practice implementing ECMAScript `Number::toString`
exactly, in every client language, is a bug factory in the one place bugs
are unrecoverable. Forbidding floats costs a string field for money.

## Migration & rollout

`c0mpute-envelope` lands alongside the v1 types in `c0mpute-net::topics`,
which keep working. Phase 2 ports the market path onto the new types and
the v1 types are removed once nothing publishes them. There is no dual-
signing window: the two formats are different message types on different
topics, so a node speaking only one is simply not a peer for the other.

Reverting means not adopting the new topics; nothing existing changes
behaviour on this DIP alone.

## Open questions

- **Key rotation.** The DID is the key, so rotating it drops the peer's
  receipt history. A signed rotation record linking old to new is the
  obvious answer and is not designed yet.
- **Advert lifetime.** 15 minutes is a guess balancing re-signing cost
  against stale capacity. Testnet churn data should set it.
- **Credential verification.** `Credential` carries a type, an issuer and
  a reference; how a buyer *checks* one is deliberately unspecified until
  there is a second credential type to generalize from.

## Out of scope

- Discovery, gossip topic layout, and relay/NAT traversal (Phase 2).
- Buyer-side scheduling policy. The crate implements the checks where
  accepting an offer would be a *bug*; price/latency/reputation weighting
  is policy and lives in the scheduler.
- Reputation scoring itself. This DIP makes receipts verifiable so that
  scoring can be derived and disagreed with; it does not pick a formula.
- The settlement adapter interface. `SettlementAdapter` names the rail on
  the wire; what an adapter must *do* is its own DIP.
