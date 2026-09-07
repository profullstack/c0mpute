# `c0mpute.job/v2`

**Protocol requirement.**

The job manifest is the network's central portable abstraction, and the
place where *buy execution, not machines* stops being a slogan.

A manifest says what to run, what the provider must have, how much the
buyer will pay, how the result gets checked, and who settles it. It does
**not** name a provider, a scheduler, or a host. That absence is the
design: the same manifest is schedulable by a local CLI, by a managed
gateway the buyer explicitly delegated to, or by a private organization
scheduler — and none of them is privileged over the others.

## Example

A real verified vector — requester seed `0x11` × 32, job id
`blake3:4a4323c24e72bb676e575acb33051450e930e499b7b68d6a41ad7d47f5551b35`.
Pretty-printed; the canonical form is compact.

```json
{
  "v": 1,
  "type": "c0mpute.job/v2",
  "signer": "did:c0mpute:z6MktULudTtAsAhRegYPiZ6631RV3viv12qd4GQF8z1xB22S",
  "payload": {
    "workload": { "type": "infernet.inference", "version": ">=1 <2" },
    "requester": "did:c0mpute:z6MktULudTtAsAhRegYPiZ6631RV3viv12qd4GQF8z1xB22S",
    "requirements": {
      "gpu": { "count": 1, "vramGiB": { "min": 24 }, "features": ["cuda"] },
      "memoryGiB": { "min": 32 },
      "region": ["us-west"],
      "trustTier": "standard"
    },
    "input": {
      "ref": "blake3:25905ca8fac0ba3d73174c28651036f55c62751fe8482bde2c766193417a57fb",
      "encryption": "recipient-selected",
      "bytes": 4096
    },
    "execution": {
      "timeoutSeconds": 120,
      "retry": 2,
      "isolation": "container",
      "runtimeDigest": "blake3:96eba7abacf8d50990a80b55e4b86374ff0f212d5441627cca6a28b6a506c550",
      "network": false
    },
    "economics": {
      "maxPrice": { "amount": "0.10", "currency": "USD" },
      "settlement": "coinpay",
      "escrow": true
    },
    "validation": { "level": "spotcheck", "redundancy": 1, "spotcheckPercent": 5 },
    "output": { "retentionSeconds": 3600, "maxBytes": 10485760 },
    "announcedAt": "2026-09-06T16:00:00.000Z",
    "deadline": "2026-09-06T16:10:00.000Z"
  },
  "sig": "z3ieWJFJSdVpcQ8GKyh5YeaDyS2mkyJCcDidR8BruTQJLV2Wah6Haio2BzoJhxRSCQZsq31cWwbWRumAXaMyPN5B6"
}
```

## Job identity

**A job's id is the content hash of its canonical manifest** — the
payload, not the envelope.

The v2 direction sketches an `id` field *inside* the manifest. That forces
every implementation to agree on which fields to strip before hashing, and
a disagreement there means two nodes calling the same job by different
names. Hashing the whole manifest deletes the step and its bugs.

Hashing the payload rather than the envelope keeps the id stable
independent of who sealed it — the manifest already names its requester,
so it is bound to the buyer either way.

```text
job id = blake3(canonical_json(payload))
```

## Requester vs signer

The manifest names its `requester`, and the envelope names its `signer`.
They are normally the same identity, and an implementation must check
that they are: an envelope proves *someone* signed the manifest, and only
the comparison proves it was the buyer the manifest names.

The field is carried in the payload rather than inferred from the envelope
because a manifest quoted inside an offer or a receipt, stripped of its
envelope, still needs to say where it came from.

## Fields

| field | required | |
|---|---|---|
| `workload.type` | yes | namespaced workload, e.g. `infernet.inference` |
| `workload.version` | no | semver range, e.g. `">=1 <2"` |
| `requester` | yes | must match the envelope signer |
| `requirements` | no | eligibility constraints; see below |
| `input` | no | content-addressed input; omitted for jobs with none |
| `execution` | yes | timeout, retries, isolation, runtime pin |
| `economics` | yes | price cap, settlement rail, escrow |
| `validation` | no | defaults to schema validation, redundancy 1 |
| `output` | no | defaults to 1 hour retention, 10 MiB |
| `announcedAt` / `deadline` | yes | the scheduling window |

## Input is content-addressed

```json
"input": { "ref": "blake3:2590…", "encryption": "recipient-selected" }
```

A content hash rather than a mutable URL. The provider can verify it
fetched what the buyer signed for, and the bytes can be served by a local
cache, a c0mpute storage provider, or an HTTP gateway without changing the
manifest or its id. The same hash written as a URI is
`c0://blake3:2590…` — it names no host, which is what lets it survive any
one host disappearing.

`encryption` absent means plaintext. Appropriate for public inputs; not
for private work.

## Execution defaults are conservative

```json
{ "isolation": "container", "network": false, "retry": 0 }
```

Network access is **off unless asked for**, and isolation defaults to a
container. A provider running untrusted work should have to opt in to
loosening either. `runtimeDigest` pins the image or plugin artifact by
digest — with the input hash, that is what makes a result reproducible by
a third party: *this input, through exactly this code, produced this
output*.

Retries are capped at 10, and the timeout must fit inside the window
between `announcedAt` and `deadline` — a manifest that cannot possibly
complete in time is rejected at signing rather than discovered by a
provider that already started work.

## Settlement is named, not assumed

```json
"settlement": "coinpay"
```

An open string. `coinpay` is the default and the first-party integration;
`x402`, `lightning`, `invoice`, `gateway-credits` and `private` are
recognized names, and an unknown lowercase-kebab value is valid — a
settlement rail nobody has built yet still travels correctly today.

This is the mechanism that keeps protocol adoption from being contingent
on adopting one payment product.

## Trust and the `private` tier

`requirements.trustTier` filters providers ([see
providers.md](providers.md#trust-tiers)). One rule is enforced rather than
documented: the `private` tier **requires** a non-empty `providers`
allowlist. Without one it silently degrades into "any provider that claims
the private tier", which is the opposite of what the buyer asked for.

## Deadlines

A job past its `deadline` is not schedulable, and offers referencing it
should be dropped. Verification still succeeds — the manifest is a record
of what was asked for, and that does not stop being true.

## Reference

`node/crates/c0mpute-envelope/src/job.rs`.
