---
dip: 0026
title: "Run Infernet temporal policies (liquid networks) as c0mpute workloads"
status: Accepted
authors:
  - riotcoder@profullstack.com
created: 2026-10-26
updated: 2026-10-26
discussion:
implementation: node/crates/c0mpute-core/src/temporal/, node/crates/c0mpute-cli/src/temporal.rs
supersedes:
superseded-by:
---

## Summary

Workers can train and serve Infernet's temporal-policy models (CfC and LTC
liquid neural networks from ncps, with a GRU baseline) through two workloads,
`infernet.temporal.train` and `infernet.temporal.infer`, on the existing
offer, bid, accept and receipt protocol. c0mpute runs no model code: it asks
the local `infernet` what it can execute and drives `infernet model train`
and `infernet model serve --stdio`. There is no new scheduler and no new
marketplace plugin; `plugins/infernet/module.toml` lists the two workloads.

## Motivation

The W3BS Liquid Agent pipeline (Infernet `docs/liquid-agent/`) learns small
UI policies from consented desktop demonstrations. Training is cheap enough
for one CPU, so the network's job is to let a user borrow a trusted machine
and to keep a stateful policy's hidden state on one worker while it serves a
session.

## Detailed design

Opt in on a worker (nothing is installed implicitly):

```bash
infernet model runtime setup --yes                       # operator's choice
c0mpute infernet temporal serve --train                  # accept training jobs
c0mpute infernet temporal serve --model sha256:<hash>    # serve a model
```

Capability tags (advertised only for what `infernet model capabilities --json`
reports): `infernet:temporal:train`, `infernet:temporal:infer`,
`infernet:temporal:runtime[:<version>]`, `infernet:temporal:device:{cpu,cuda,mps}`,
`c0mpute:ram:<n>gb+` buckets, and `infernet:temporal:model:<sha256-hex>` for
each loaded model. Buyers turn requirements (GPU, minimum RAM, runtime
version, model hash) into `required_capabilities`; the existing dispatcher
filter does the matching. An `OfferFilter` adds per-workload checks before a
bid: a train worker validates the spec and does not bid while it is busy
(one job per node); an infer worker bids only for models it holds.

Training (`c0mpute infernet temporal train`): one single-node job per family
(CfC plus baselines). The spec carries a liquid-agent/v1 `TrainJobV1`, a
token-gated dataset URL served by the buyer, the dataset hash and an upload
URL. Workers refuse a job unless `privacy.mode = trusted_worker` and
`upload_approved = true`; the CLI refuses to send without `--yes` after a
preview. `--worker <peer-id>` restricts bids to trusted workers. The worker
runs `infernet model train --job` (which re-verifies every hash and the
secret scan), uploads `manifest.json`, `model.safetensors` and `eval.json`,
and receipts with the model hash. The buyer accepts the artifacts only if
the weights hash equals the receipt's `output_hash` and the manifest's.
Repeated idempotency keys reuse the finished model; failed or timed-out jobs
are re-auctioned up to `--retries` times.

Inference (`c0mpute infernet temporal predict --session <id>`): the first call
auctions a session open; the winner opens the session in its runtime,
publishes its X25519 key half and receipts with the model hash. Later calls
are AES-256-GCM sealed with an HKDF key bound to the session and go only to
that worker, which answers only the peer that opened the session (gossipsub
signs the source). A call id cache gives exactly-once execution when the
buyer republishes. Sequence rules (duplicate, late, gap), TTL, capacity and
account scope are enforced by the Infernet runtime.

## Compatibility

Additive. Workers that do not run `temporal serve` never see these topics.
`c0mpute infernet <anything else>` still delegates to the infernet binary.

## Security and privacy

- A worker sees the redacted training data in plaintext; transport security
  does not change that. Local training is the default upstream; remote
  training is an explicit, previewed choice of a trusted worker.
- The dataset URL is gated by a random token; use `--public-url https://...`
  off the LAN. Artifacts are verified by hash, not trusted.
- Session traffic is encrypted between buyer and worker; other subscribers of
  the topic see only ciphertext and routing ids.

## Not done

- Cross-worker session failover (checkpoint transfer) over the network; the
  runtime supports checkpoint/restore locally.
- Price discovery beyond the existing cooperative bid, and settlement wiring
  for these receipts.
