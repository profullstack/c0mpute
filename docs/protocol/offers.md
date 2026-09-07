# `c0mpute.offer/v1`

**Protocol requirement.**

An offer is a provider's binding quote for one specific job, and it is
where the v2 direction's central architectural claim gets cashed out:

> The offer is **signed by the provider** and **selected by the buyer**.
> Nothing in between decides.

A gateway may collect offers on a buyer's behalf. An indexer may have
suggested which providers to ask. Neither can manufacture an offer, and
neither gets to pick.

## Example

A real verified vector — signer seed `0x77` × 32, hash
`blake3:9e9151a1d682f3c60fb596b92dad5757be890f476cf979dbe8c924228494393f`.

```json
{
  "v": 1,
  "type": "c0mpute.offer/v1",
  "signer": "did:c0mpute:z6MkswFb62xmEDrqnknM3TP112AiH6A5YETp7gc2Qz4Wqkar",
  "payload": {
    "job": "blake3:4a4323c24e72bb676e575acb33051450e930e499b7b68d6a41ad7d47f5551b35",
    "price": { "amount": "0.042", "currency": "USD" },
    "expectedDurationMs": 12000,
    "startBefore": "2026-09-06T16:02:00.000Z",
    "expiresAt": "2026-09-06T16:00:10.000Z",
    "capabilityAdvert": "blake3:cecfc7a08527ee50ab8945bb636afc940b5afff48a50cdfbcdee501edbebd2c2",
    "coordinator": false
  },
  "sig": "z3J1bxFxvwCni6Hg6PHHas1wLCGLWNn1YrZNSNb4dH8nvQJFWg85FgqKeAYMkD1JTUVhhmHGBHuxrjrdunS1p8pfD"
}
```

## Fields

| field | required | |
|---|---|---|
| `job` | yes | the job id being bid on |
| `price` | yes | binding, unlike an advert's indicative rate |
| `expectedDurationMs` | yes | must be > 0 |
| `startBefore` | yes | the provider commits to starting by this instant |
| `expiresAt` | yes | quote void after this; must be ≤ `startBefore` |
| `capabilityAdvert` | no | the advert this quote is backed by, by hash |
| `coordinator` | no | willing to coordinate a multi-provider job; default `false` |

## The checks a buyer must not skip

Three constraints where accepting a failing offer is simply a **bug**, not
a policy choice:

1. **Right job.** The offer's `job` matches the manifest's id.
2. **Within the cap.** `price ≤ economics.maxPrice`, compared *by value*.
   Lexically `"0.042" > "0.10"`; numerically it is not. A string
   comparison here would reject good offers and accept `"0.9"` against a
   `"0.10"` cap.
3. **Fits the deadline.** `startBefore + expectedDurationMs ≤ deadline`.

Currencies must match. There is no exchange rate at the protocol layer, so
a cross-currency comparison is an **error**, never a silent `false` — the
protocol must not guess a rate, and a buyer that meant to accept another
currency should say so.

Everything else — reputation, latency, provider diversity, jurisdiction,
prior experience, validation cost — is **policy**, and lives in the
scheduler rather than here. That is the split: this page defines what
makes an offer *valid*, not what makes it *the best one*.

## Offers go stale by construction

A quote is a promise about capacity the provider has **now**. Letting it
linger means bidding on hardware that is already busy, so
`expiresAt` must not be later than `startBefore`: a quote you can still
accept after its promised start time is not binding.

The example above has a ten-second bid window against a two-minute start
commitment, which is the shape to expect for interactive work.

## `capabilityAdvert` ties price to claims

Naming the backing advert by hash lets a buyer connect the price to the
capabilities it was quoted against, and lets the eventual receipt record
which claim of capacity was relied on. If a provider advertised 24 GiB of
VRAM, quoted against that advert, and then failed the job, the record
shows all three facts without anyone having to be believed.

## `coordinator` is per-job, never a role

A multi-provider workload — distributed inference, say — needs one
participant to coordinate. A provider signals willingness per offer.

The coordinator elected for one job has **no authority over any other
job**. That is the whole difference between an ephemeral coordinator and a
control plane, and it is why the flag lives on an individual offer rather
than in a provider's advert or in a registry somewhere.

## Reference

`node/crates/c0mpute-envelope/src/offer.rs`.
