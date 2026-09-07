# Envelopes

**Protocol requirement.**

Every c0mpute protocol message travels in a signed envelope:

```json
{
  "v": 1,
  "type": "c0mpute.provider.advert/v1",
  "signer": "did:c0mpute:z6MkswFb62xmEDrqnknM3TP112AiH6A5YETp7gc2Qz4Wqkar",
  "payload": { },
  "sig": "z5p6RP1aX1ZAeffARCUXgBAJ12RBphawNMKnSQLhF4UWU9T8RXE5BUyALwFJqYvzHQP6kuYwCFAfckividdjCHL5B"
}
```

| field | |
|---|---|
| `v` | envelope format version, currently `1` |
| `type` | payload type identifier |
| `signer` | the DID that vouched for the payload |
| `payload` | the message |
| `sig` | detached signature over the domain-separated canonical bytes |

## Why an envelope, when gossipsub already signs messages

Because a transport signature dies at the first hop.

libp2p signs each pubsub message with the publisher's key, which is
sufficient for exactly as long as the message is on the wire. It does not
help with:

- an advert **relayed by an indexer** — arrives with no provenance;
- a receipt **read back from local disk** after a restart;
- an offer **forwarded by a gateway** — the gateway's word;
- a record shown to **a third party** who was not on the wire at all.

The v1 market types in `c0mpute-net::topics` carry no signatures of their
own and rely on transport authenticity. That is precisely what makes
reputation non-portable: a score computed from unverifiable receipts has
to be *believed*, which means a trusted database ends up in the middle of
a network that is supposed not to need one.

An envelope makes a record verifiable by anyone holding it, forever,
without asking anybody.

## The signing input

```text
"c0mpute-envelope/v1\n" ‖ type ‖ "\n" ‖ canonical_json({v, type, signer, payload})
```

Two layers of domain separation:

- the **tag** `c0mpute-envelope/v1\n` keeps a c0mpute signature from ever
  being meaningful in another protocol that reuses the same key;
- the **type**, prefixed *and* covered inside the canonical JSON, keeps a
  signature harvested from one message type from being replayable as
  another.

That second one matters more than it looks. Two payload types can have
identical field shapes — a `{"note": "…"}` in one type is byte-identical
to a `{"note": "…"}` in another — and without type separation, lifting the
signature from one and attaching it to the other would verify. The
reference implementation tests exactly this case.

## Verification order

`open()` checks, in order:

1. **envelope version** — an unknown `v` is refused rather than guessed at;
2. **payload type** — matches what the caller expected;
3. **signature** — over the reconstructed signing input;
4. **payload structure** — the payload's own validity rules.

Only then is the payload returned. Nothing downstream should read
`payload` directly: going through `open()` is what keeps "we received
this" and "this peer signed this" the same statement.

Structural validation also runs **before** signing. A signature is a claim
that the signer stands behind the contents, and there is no reason to
stand behind a payload already known to be malformed.

## Freshness is separate from authenticity

`open()` verifies. `open_fresh(now)` verifies *and* enforces the payload's
own expiry.

The split is deliberate. Adverts and offers expire; on a churning network
that is the normal case, not an attack — a provider that loses power
leaves its last advert sitting in every peer's cache. Rejecting expired
records on read is what stops a stale cache from looking like live
capacity. But an expired advert is **history, not a forgery**: it should
still verify, because reconstructing what a provider claimed last Tuesday
is a legitimate thing to do.

Receipts never expire. A two-year-old receipt is exactly as valid a
statement about the past as a fresh one.

## Content hash

An envelope's content hash is the BLAKE3 of its canonical JSON, written
`blake3:<hex>`. Because canonicalization is deterministic and ed25519 is
deterministic, two nodes holding the same envelope compute the same hash —
so a record can be *cited* by hash without being re-sent, which is how
offers reference jobs and receipts reference offers.

Note the one asymmetry: a **job id** is the hash of the job manifest
*payload*, not of its envelope. See [jobs.md](jobs.md#job-identity).

## Versioning

The envelope's `v` changes only for a breaking change to the envelope's
own shape. Payload changes move the version inside the type identifier
(`c0mpute.job/v2` → `c0mpute.job/v3`), which means a payload version bump
is automatically a different domain-separation string — old signatures
cannot carry over to a new payload version even by accident.

## Reference

`node/crates/c0mpute-envelope/src/envelope.rs`.
