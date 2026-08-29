---
dip: 0012
title: "c0mpute is compute-only; we don't run a storage network"
status: Superseded
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-08-29
discussion:
implementation:
supersedes:
superseded-by: DIP-0012 (0012-storage-plugin.md)
---

> **Superseded.** This is draft v1 of DIP-0012 and no longer describes the
> project's position. It was withdrawn in favour of
> [`0012-storage-plugin.md`](0012-storage-plugin.md), whose motivation table
> records this draft and why it was dropped — the file simply never had its
> status updated, leaving two `Accepted` DIP-0012s on disk asserting opposite
> things.
>
> **c0mpute hosts files.** See the storage-plugin DIP for the current design,
> and [`docs/prds/`](../docs/prds/README.md) (CIP-001 onward) for the delivery
> plan. The cost analysis below is still worth reading: the five structural
> overheads it identifies are real, and CIP-001 and CIP-005 are largely
> answers to them.

## Summary

c0mpute is a **compute marketplace**, not a storage network. Customers
bring their own storage (S3, R2, B2, IPFS, anything addressable by URL)
for both inputs and outputs of jobs. Workers hold data only ephemerally
during a job.

If demand emerges for decentralized storage of job outputs, we add it
as a **plugin** that wraps an existing decentralized storage network
(Storj, Filecoin, or whatever wins) — we don't build that layer
ourselves.

## Motivation

The original Quest PRD positioned the network as both compute *and*
CDN/storage. The v1 c0mpute pivot dropped that — and skepticism about
whether anyone has actually nailed cents-on-the-dollar p2p storage was
the forcing function. The honest answer to "can we hit B2's
$0.006/GB-month with proof-of-replication overhead?" is "nobody has,
and the reasons are structural, not technological."

### Why p2p storage doesn't reach cents-on-the-dollar

Five overheads that eat the margin in any permissionless storage
network:

1. **Verification.** Proving "node X still has chunk Y" costs
   network + CPU. Filecoin's PoRep/PoSt requires staked collateral
   to make the math work. Cheap challenges scale better but you need
   thousands per chunk per month to keep providers honest.
2. **Egress.** Customer downloads cost the network bandwidth.
   Cloudflare R2's $0 egress is possible because Cloudflare owns
   global backbone. Home/prosumer nodes don't get free bandwidth and
   most ISPs cap residential uploads.
3. **Churn.** Home nodes go offline. Every disappearance triggers
   re-replication = more egress + new commitments + new verification.
   At small network size this dominates steady-state cost.
4. **Hot-vs-cold tiering.** Hot serving requires nodes with reliable
   uptime + good bandwidth (effectively datacenter-class). Cold is
   cheap but useless for CDN. Most p2p networks pick one and do badly
   at the other.
5. **Per-object metadata.** Hash + location + sigs + expiry. At small
   chunk sizes this overhead dominates; Filecoin uses 32 GB sectors
   to amortize, which is hostile to small files.

### What the existing networks actually do

| Network | Real $/GB-month | What makes it work |
|---|---|---|
| Storj | ~$0.004 | Workers eat their own bandwidth; centralized metadata coordinator |
| Sia | ~$0.005 | Same shape, smaller |
| Filecoin | $0.001–0.01 | Only when retrieval is slow and deals are huge |
| IPFS | $0 + your hw | "Pinning" is the actual cost — network doesn't store anything |

None hit truly permissionless + cheap + fast all at once. The
economics force you to pick two.

## Detailed design

### v1 storage model: customer-provided URLs

Job manifests reference inputs and outputs by URL:

```json
{
  "input": {
    "uri": "https://customer-bucket.s3.amazonaws.com/input.mov?signed=...",
    "sha256": "..."
  },
  "output": {
    "uri": "https://customer-bucket.s3.amazonaws.com/output.mp4?signed=...",
    "format": "mp4"
  }
}
```

The worker:
1. Downloads input from the signed URL.
2. Verifies the sha256 (the customer commits to a hash so the worker
   can detect tampering).
3. Runs the workload locally on ephemeral disk.
4. Uploads output to the customer-provided destination URL.
5. Signs a receipt covering input hash + output hash + runtime image
   hash, returns it through CoinPay.

Working data on the worker is ephemeral — wiped after the job (or
after a configurable retention window for re-verification spot checks
per DIP-0011).

### What this rules out

- No `c0mpute://blake3:...` content-addressed network URLs (was
  Quest concept; doesn't survive the compute-only pivot).
- No CDN-style serving of transcoded output by c0mpute gateways.
  Customer plugs in Cloudflare / Bunny / etc. for delivery.
- No persistent storage as a c0mpute network primitive.

### When storage might come back

Three triggers would cause us to revisit:

1. **A customer pays for it.** Specifically, asks for c0mpute to host
   transcoded output and is willing to pay storage rates that cover
   the operational reality.
2. **Existing networks add a clean integration point.** Storj's S3
   gateway and Filecoin's web3.storage already let us write a thin
   adapter rather than build storage from scratch.
3. **The economics shift.** A protocol-level breakthrough on cheap
   verification (e.g. recursive ZK proofs becoming routine) might
   change the cost structure enough to make our own network viable.

If any of these happens, the response is a `storage` **plugin** in
`plugins/storage/` that wraps the chosen backend — same model as
`coinpay` wrapping CoinPay's actual project, or `infernet` wrapping
infernet-protocol. **Not a core c0mpute capability.**

## Alternatives considered

**Build our own.** Discussed above — the structural overheads make
this a multi-year project with margin uncertainty. Not v1, not soon.

**Mandate IPFS for inputs/outputs.** Forces customers to learn IPFS
and pin their own data. Adds friction with no clear benefit over S3
URLs, and IPFS doesn't actually solve the durability problem.

**Run S3-compatible gateway nodes ourselves as a "compatible" tier.**
Tempting but it's just centralized object storage with extra steps —
we'd be reinventing R2 with worse economics. Skip.

## Migration & rollout

This DIP locks the position; nothing to migrate. Specifically:

- The `c0mpute-store` Rust crate stays, but only as **per-node
  ephemeral cache** for working data. It is not the basis of a future
  storage network.
- The original Quest PRD's references to "chunk replication", "Reed-
  Solomon erasure coding", and the storage-verification challenges in
  the c0mpute-verify crate are dormant — kept as design reference but
  not a roadmap commitment.
- The v1 PRD already describes the customer-URL model in the job
  manifest section; this DIP confirms that's not a stopgap.

## Open questions

- **Receipt anchoring.** Job receipts (signed by worker + validator
  DIDs through CoinPay) need somewhere durable. CoinPay's own ledger
  is the assumed home — verifying that assumption is a CoinPay-side
  question, not c0mpute's.
- **Public attestation of completed jobs.** If/when customers want a
  public log of "yes, c0mpute job XYZ ran successfully", we'd want
  some persistent surface. Probably CoinPay receipts → public
  aggregator. Out of scope here.

## Out of scope

- The future storage plugin's design (deferred until trigger #1
  actually happens).
- Whether c0mpute itself should ever offer paid CDN-style serving of
  customer outputs. (No, per this DIP — but a third-party plugin
  could.)
- IPFS / Filecoin / Storj integration specifics — not until a
  customer asks.
