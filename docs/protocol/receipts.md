# `c0mpute.receipt/v1`

**Protocol requirement.**

A receipt is the network's durable record of what happened, and the input
to every reputation calculation.

> **Reputation is derived, not stored.**

There is no global score and no authoritative table holding one. A peer
that has collected receipts computes a provider's completion rate, dispute
rate and offer accuracy for itself, and two peers weighting those
differently are both correct. That is what makes reputation survive an
operator's database going away — or an operator deciding it does not like
you.

## Example

A real verified vector — signer seed `0x77` × 32, hash
`blake3:7c5ba273a840b6bfd859f24904e032229010cee3de67c4102e194d3c558e87df`.

```json
{
  "v": 1,
  "type": "c0mpute.receipt/v1",
  "signer": "did:c0mpute:z6MkswFb62xmEDrqnknM3TP112AiH6A5YETp7gc2Qz4Wqkar",
  "payload": {
    "job": "blake3:4a4323c24e72bb676e575acb33051450e930e499b7b68d6a41ad7d47f5551b35",
    "offer": "blake3:9e9151a1d682f3c60fb596b92dad5757be890f476cf979dbe8c924228494393f",
    "buyer": "did:c0mpute:z6MktULudTtAsAhRegYPiZ6631RV3viv12qd4GQF8z1xB22S",
    "provider": "did:c0mpute:z6MkswFb62xmEDrqnknM3TP112AiH6A5YETp7gc2Qz4Wqkar",
    "workload": "infernet.inference",
    "inputHash": "blake3:25905ca8fac0ba3d73174c28651036f55c62751fe8482bde2c766193417a57fb",
    "outputHash": "blake3:4ceae457499f31d30cdc14ecf51de929a384100db61a782bbcae38cc7f99a401",
    "runtimeHash": "blake3:96eba7abacf8d50990a80b55e4b86374ff0f212d5441627cca6a28b6a506c550",
    "startedAt": "2026-09-06T16:01:00.000Z",
    "completedAt": "2026-09-06T16:01:12.000Z",
    "price": { "amount": "0.042", "currency": "USD" },
    "validation": {
      "policy": { "level": "spotcheck", "redundancy": 1, "spotcheckPercent": 5 },
      "status": "accepted"
    },
    "settlement": {
      "adapter": "coinpay",
      "reference": "cp_rcpt_01J8Z3",
      "settled": true
    }
  },
  "sig": "z2F57B4GSYCWbNuayafMTWWyKELs3ZwwL6gXwx9XWctioRnbXWimiKDK8RUSwjDm5pgSvUb3nbD8HieQiSDwn3tRH"
}
```

## Fields

| field | required | |
|---|---|---|
| `job` | yes | the job id |
| `offer` | yes | the offer that set the price, by hash |
| `buyer` / `provider` | yes | must differ |
| `validator` | no | the independent validator, at levels L2 and up |
| `workload` | yes | namespaced workload |
| `inputHash` | no | what went in |
| `outputHash` | no | what came out; absent on failure |
| `runtimeHash` | no | the pinned runtime that produced it |
| `startedAt` / `completedAt` | yes | `completedAt` must not precede `startedAt` |
| `price` | yes | what was charged |
| `validation` | yes | the policy applied and what it concluded |
| `settlement` | yes | adapter, external reference, and whether it settled |

## Everything links by hash

The receipt names the job **and** the offer that priced it. That is what
makes the price checkable rather than assertable: a third party holding
the offer and the receipt can confirm the amount charged is the amount
quoted, by a provider that signed the quote before doing the work.

With `inputHash` and `runtimeHash`, a receipt also states *this input,
through exactly this code, produced this output* — which is what makes a
result reproducible by someone who trusts none of the participants.

## Failures are receipts too

A timeout, a crash, or a provider withdrawal produces a receipt with no
`outputHash` and a status of `failed`. A provider's failures are part of
its record, and a network where only successes are signed has a reputation
system that measures nothing.

Two rules are enforced:

- an `accepted` result **must** record an output hash;
- `settled: true` is only valid alongside `accepted` — only accepted work
  settles.

## Validation status

| status | |
|---|---|
| `accepted` | passed; the job should settle |
| `rejected` | failed validation; no settlement |
| `disputed` | the parties disagree; settlement held |
| `failed` | no result to validate |

The `policy` is repeated inside the receipt so that a reader holding only
the receipt knows *how hard the result was actually checked*. "Accepted"
under `requester` (L0) and "accepted" under `quorum` (L3) are very
different claims, and a reputation calculation that treats them alike is
wrong.

## Settlement is recorded, not performed

```json
"settlement": { "adapter": "coinpay", "reference": "cp_rcpt_01J8Z3", "settled": true }
```

`reference` is the adapter's own identifier — a CoinPay receipt id, a
Lightning payment hash, an invoice number. **Opaque on purpose**: the
protocol records *that* an adapter settled and how to look it up, and
knows nothing about any adapter's internals. That is what keeps a new
settlement rail from being a protocol change.

## Two signatures, two envelopes

A receipt needs the provider's signature and the buyer's. Rather than one
message carrying a `signatures` object, the provider seals the receipt and
the buyer seals a separate `c0mpute.receipt.acceptance/v1` naming it by
hash:

```json
{
  "v": 1,
  "type": "c0mpute.receipt.acceptance/v1",
  "signer": "did:c0mpute:z6MktULudTtAsAhRegYPiZ6631RV3viv12qd4GQF8z1xB22S",
  "payload": {
    "receipt": "blake3:7c5ba273a840b6bfd859f24904e032229010cee3de67c4102e194d3c558e87df",
    "accepted": true,
    "signedAt": "2026-09-06T16:01:20.000Z"
  },
  "sig": "z5PDHRVens9NQoXoxc3ZgzBKm1hZVrwo4CYV5dyi1MZdrdDbE1kogourH4CRhaYmZHfNs1GDCrHKmtoQye36rFX2y"
}
```

Three reasons for the split:

- the signatures are made at different times by parties who may never be
  online together, so **one arriving without the other is normal**, not a
  malformed message;
- a buyer needs to be able to say **no**, and a countersignature field has
  no way to express a rejection;
- both reuse one signing and verification path, so there is no second,
  subtly different way to check a signature.

A dispute (`accepted: false`) **must** state a reason. An unexplained
dispute cannot be weighed against a provider's record, so allowing one
would just be a free way to damage a provider.

## Receipts never expire

A two-year-old receipt is exactly as valid a statement about the past as a
fresh one. Adverts and offers carry expiry; receipts deliberately do not.

## Deriving reputation

Not specified yet — see the [README](README.md#not-yet-written). What the
receipt format makes *available* to a scorer:

completed jobs · completion rate · failure rate · validation failures ·
disputes · refunds · offer accuracy (quoted price vs charged) · latency
accuracy (`expectedDurationMs` vs actual, both signed, one before the work
and one after) · time on network · settlement history · validation level
achieved.

Latency and offer accuracy are the most useful and the hardest to fake:
the quote was signed before the work and the receipt after it, so a
provider that systematically under-quotes leaves a signed trail of it.

## Reference

`node/crates/c0mpute-envelope/src/receipt.rs`.
