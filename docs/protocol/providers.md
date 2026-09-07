# `c0mpute.provider.advert/v1`

**Protocol requirement.**

A provider's signed, short-lived claim about what it can run.

Anyone holding an advert can check who made it and whether it is current,
without asking a registry. That is what lets indexers cache adverts while
remaining non-authoritative: an indexer can be stale, partial, or lying
about *which* adverts exist — and cannot forge one. A buyer that doubts
the index collects adverts off the network instead.

## Example

Canonical form is compact; pretty-printed here for reading. This is a real
verified vector — signer seed `0x77` × 32, hash
`blake3:cecfc7a08527ee50ab8945bb636afc940b5afff48a50cdfbcdee501edbebd2c2`.

```json
{
  "v": 1,
  "type": "c0mpute.provider.advert/v1",
  "signer": "did:c0mpute:z6MkswFb62xmEDrqnknM3TP112AiH6A5YETp7gc2Qz4Wqkar",
  "payload": {
    "sequence": 8472,
    "issuedAt": "2026-09-06T23:50:00.000Z",
    "expiresAt": "2026-09-06T23:59:00.000Z",
    "capabilities": {
      "cpu": { "arch": "x86_64", "cores": 32 },
      "memoryGiB": 128,
      "gpus": [
        {
          "vendor": "nvidia",
          "family": "ada",
          "vramGiB": 24,
          "features": ["cuda", "nvenc"],
          "count": 1
        }
      ],
      "storageGiB": 1200,
      "bandwidthMbps": 1000,
      "workloads": ["infernet.inference", "transcode.ffmpeg"]
    },
    "pricing": [
      {
        "workload": "infernet.inference",
        "unit": "1m-output-tokens",
        "price": { "amount": "0.60", "currency": "USD" }
      }
    ],
    "trust": { "tier": "standard" },
    "disclosure": { "region": "us-west" }
  },
  "sig": "z5p6RP1aX1ZAeffARCUXgBAJ12RBphawNMKnSQLhF4UWU9T8RXE5BUyALwFJqYvzHQP6kuYwCFAfckividdjCHL5B"
}
```

## Fields

| field | required | |
|---|---|---|
| `sequence` | yes | monotonic counter; a peer holding two adverts from one provider keeps the higher |
| `issuedAt` / `expiresAt` | yes | lifetime; capped at **15 minutes** |
| `capabilities.cpu` | yes | `arch` and `cores` (≥ 1) |
| `capabilities.memoryGiB` | yes | |
| `capabilities.gpus` | no | accelerators; `count` defaults to 1 |
| `capabilities.storageGiB` | no | |
| `capabilities.bandwidthMbps` | no | |
| `capabilities.workloads` | yes | ≥ 1, unique, namespaced |
| `pricing` | no | indicative rates; every priced workload must be advertised |
| `trust.tier` | yes | see below |
| `trust.credentials` | no | assertions others have made |
| `disclosure` | no | opt-in region / country / ASN |

## Adverts expire fast

Fifteen minutes, maximum, enforced at signing. A residential provider that
loses power leaves its last advert in every peer's cache; a short lifetime
is what stops that cache from looking like live capacity. The cost is
re-signing every few minutes, which is one ed25519 operation.

An expired advert still *verifies* — see
[envelopes.md](envelopes.md#freshness-is-separate-from-authenticity).

## Workload namespaces

Lowercase, dot-separated, at least two segments: `transcode.ffmpeg`,
`infernet.inference`, `whisper.transcribe`, `oci.container`. A bare verb
(`transcode`) is rejected, because unqualified names are exactly what
different plugins would each interpret their own way.

Advertising a workload is **acceptance policy**, not capability alone.
Advertising `whisper.transcribe` and refusing `oci.container` is the
normal, expected posture for a machine its owner also uses for other
things.

## Pricing is indicative

Rates in an advert are what a buyer *filters* on. The binding number is
the one in a signed [offer](offers.md). A rate names a workload, a unit,
and a price:

```json
{ "workload": "infernet.inference", "unit": "1m-output-tokens",
  "price": { "amount": "0.60", "currency": "USD" } }
```

Units are an open string — `gpu-second`, `cpu-core-second`, `video-minute`,
`audio-minute`, `1m-input-tokens`, `1m-output-tokens`, `image`, `gb-month`,
`gb-transferred`, `request`, `reserved-capacity-hour`, `job`. Plugins
define their own; the protocol carries a unit without needing to know what
it means.

Prices are decimal **strings**, never floats
([canonical-json.md](canonical-json.md#why-no-floats)).

## Trust tiers

A tier is a **claim**, and a buyer's policy decides which claims it
credits. The network does not adjudicate.

| tier | |
|---|---|
| `community` | cryptographic identity only; no KYC; low-value work |
| `standard` | established receipt history, minimum success rate |
| `verified` | provider credential, hardware verification where available |
| `private` | on a specific buyer's allowlist |
| `enterprise` | organization verification, SLA, jurisdiction controls |

Matching is by strength, with one exception: `private` and `enterprise`
mean *"on this list"*, so they satisfy only themselves. A `verified`
provider can take `standard` work; it cannot take `private` work by being
strong, because that would defeat what `private` means.

`community` is the default. A fresh key with no history is honestly
described by it.

## Disclosure is opt-in

Every field under `disclosure` is a choice, and absent means **"not
saying"** — never "unknown". Publishing none of it is a supported way to
run a provider; the consequence is simply that a buyer requiring a region
cannot match you.

Related privacy defaults: no hostname, no exact location, no raw process
list, no unnecessary OS fingerprint. GPUs are described by vendor, family
and feature strings rather than exact model — which also keeps the
protocol from having to learn every new SKU.

## Reference

`node/crates/c0mpute-envelope/src/advert.rs`.
