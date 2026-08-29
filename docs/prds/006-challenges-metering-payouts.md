---
cip: 006
title: "Storage challenges, metering, and provider payouts"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-08-29
updated: 2026-08-29
implements: DIP-0012 (0012-storage-plugin.md) Phase 5, DIP-0007 (CoinPay DID)
depends-on: 003, 004
blocks:
implementation:
estimate: "4–6 weeks"
---

## Summary

Pay people for the disk they contribute, and make sure they are actually
contributing it. This is the half of the product that creates supply: without
payouts there are no providers, and without challenges the payouts fund fraud.

## Motivation

The whole design assumes a pool of nodes that hold shards honestly for money.
Neither the money nor the honesty exists yet. `c0mpute-verify` has the right
primitive already — `StorageChallenge` with byte-range proofs and a tested
`expected_response` — but nothing ever issues one, and no payout path exists
for storage at all.

Getting this wrong in the obvious way is expensive: a node that stores nothing,
answers `Have` with "yes", and collects $0.0015/GB-month is strictly more
profitable than an honest one, right up until a customer tries to read.

## Goals

- Prove a node holds the bytes it claims, cheaply and continuously.
- Meter stored bytes and served bytes attributably.
- Pay providers monthly through CoinPay with an auditable statement.
- Bill customers for storage, egress, and operations.
- Make cheating cost more than it earns.

## Non-goals

- Staking, bonding, or slashing deposits. Reputation and payout forfeiture are
  the levers; CIP-001 explicitly rules out collateral.
- Fiat payouts or tax handling — CoinPay's problem, not ours.
- Preventing Sybil identity creation (accepted residual risk, CIP-003).

## Design

### Challenges

A challenge asks: *hash bytes `[offset, offset+len)` of shard `S`*. The
verifier knows the answer because it can compute it from any `k` shards of the
block; the holder can only answer by possessing the shard.

```rust
pub struct StorageChallenge {
    pub chunk_hash: c0mpute_proto::Hash,
    pub offset: u32,
    pub length: u32,
}
```

Already implemented and tested. What's missing is issuance, scheduling,
response verification, and consequences.

**Who challenges.** Peer holders of the same block, on the same rendezvous
rotation as CIP-005's repair election but offset by round, so challenging and
repairing responsibilities spread evenly. The customer's client also challenges
opportunistically when online — but the network must not depend on it.

**Rate.** Each shard is challenged on average once per 24 hours, with the
offset chosen from a per-round seed the holder cannot predict in advance. A
32 KiB range read plus a hash is negligible for an honest node and impossible
to fake without the data.

**Precomputation attack.** A node that stores only *answers* to past challenges
rather than the shard defeats a fixed challenge set. Defence: the range is
derived from `blake3(shard_hash || round_seed || challenger_did)` where
`round_seed` comes from a recent gossip beacon, so the answer space is
unbounded and unpredictable. Storing enough precomputed answers costs more than
storing the shard.

**Response deadline** 5 seconds. Late is a fail; a node that has the data but
cannot serve it inside 5s is not providing a usable service.

**Consequences**, graded — most failures are innocent (reboots, flaky
connections), so the ladder starts gently:

| Pass rate (30d) | Effect |
|---|---|
| ≥ 99% | Full payout, eligible for new placements |
| 95–99% | Full payout, no *new* placements until recovered |
| 80–95% | Payout scaled by pass rate; shards drain via repair |
| < 80% | Payout forfeit for the period; shards drained; role suspended |

Failures feed `verification_pass_rate` in the existing
`c0mpute-verify::reputation` formula, which already weights it at 0.15 and
carries a 0.50 slash term.

### Metering

Two meters, both needing to be attributable and hard to inflate.

**Stored bytes.** Sampled hourly: for each shard a node holds and has passed a
challenge on within the window, accrue `bytes × hours`. Accrual requires a
recent passing challenge, so claiming storage you don't have earns nothing.
Data held past deletion during CIP-004's GC grace period **is** billable to the
network but **not** to the customer — that is a real cost of the decentralised
GC design, absorbed by margin. It is small (14 days of a fraction of deleted
data) but it must be budgeted rather than discovered.

**Served bytes.** The serving node reports bytes served, and the *receiving*
party countersigns a receipt. Neither side can inflate alone:

```json
{
  "server_did": "did:coinpay:...",
  "client_did": "did:coinpay:...",
  "object": "blake3:...",
  "bytes": 4194304,
  "internal": true,
  "served_at_ms": 1756512000000,
  "server_sig": "...", "client_sig": "..."
}
```

`internal: true` means the reader was a c0mpute worker, which is billed to the
customer at $0 (CIP-001's compute-locality promise) but still **paid to the
provider** at the egress rate. That asymmetry is deliberate and is a genuine
cost centre: internal reads earn nothing and cost egress payouts. It is the
price of the differentiator, and it must be sized before launch — if internal
reads dominate, the $0-internal-egress promise needs a fair-use ceiling.

Repair traffic generates receipts marked `repair: true`, which are **measured
but not paid** (CIP-001). Measuring them anyway is what makes CIP-005's
incentive problem visible.

### Payouts

Monthly, through CoinPay, to the provider's DID.

```
payout = stored_gb_month × $0.0015 × pass_rate_multiplier
       + customer_egress_gb × $0.002
       - penalties
```

Statement available via `c0mpute storage earnings [--month YYYY-MM]`, itemised
by volume-agnostic aggregate (never revealing which customer's data a node
holds). Minimum payout threshold $5, rolling over below that.

### Customer billing

```
bill = Σ tier_price × gb_month           (CIP-001 tier prices)
     + internet_egress_gb × $0.004
     + $0 for internal egress
     + $0 for operations
```

Per-operation charges are deliberately zero — the pricing analysis in
`docs/storage-pricing-scenarios.md` already found they round to noise below
billions of requests, and "no per-request fees" is a cleaner promise than
matching R2's $0.36/M.

Billing runs off the same signed receipts as payouts, so the two reconcile by
construction. Any gap between what customers are billed and what providers are
paid is margin, and it should be continuously observable rather than computed
at month end.

### Fraud

| Attack | Defence |
|---|---|
| Claim storage, store nothing | Challenges; accrual gated on passing |
| Store one copy, claim `n` shards under `n` identities | Diversity constraints make co-location detectable; challenge all `n` simultaneously and measure response correlation |
| Inflate served bytes | Countersigned receipts; the client won't sign what it didn't get |
| Collude: operator runs both server and client, signs fake receipts | Egress payout capped at a multiple of stored bytes; anomalous ratios flagged. **Not fully solved** — see risks |
| Store, pass challenges, refuse real reads | Read failures are reported and feed `job_completion_rate` |
| Precompute challenge answers | Unpredictable seeded ranges |

## Acceptance criteria

1. A node deleting a shard it claims fails its next challenge within 24h, and
   accrual for that shard stops.
2. Challenge cost is under 0.1% of a node's bandwidth at 10 TB held.
3. A node with a precomputed answer table for 1000 past challenges still fails
   new ones.
4. Served-byte receipts require both signatures; a single-signed receipt is
   rejected by the billing reconciler.
5. `c0mpute storage earnings` matches an independent recomputation from raw
   receipts to the cent.
6. A node at 97% pass rate keeps full payout but receives no new placements.
7. A node at 70% is drained, suspended, and forfeits the period.
8. Customer bill and provider payouts reconcile: `Σ bills − Σ payouts` equals
   expected margin within rounding.
9. A brief outage (2h) does not measurably reduce a node's monthly payout.

## Risks

- **Self-dealing egress collusion is not fully solved.** An operator running a
  node and a client can sign receipts for reads that never happened, minting
  egress payouts. The cap on egress-to-stored ratio limits the damage but does
  not eliminate it. *Mitigation:* cap, anomaly detection, and manual review
  above a payout threshold. This is a known open problem in every p2p storage
  network; do not claim it is solved.
- **Challenge traffic at scale.** Every shard, daily, network-wide, is a lot of
  messages. *Mitigation:* batch challenges per peer-pair; challenge at block
  granularity, sampling one shard per block per round.
- **Providers churn out when they see real earnings.** $0.0015/GB-month means a
  fully-utilised 4 TB drive earns ~$6/month. That is idle-resource arbitrage,
  not a business, and the messaging must say so plainly — a provider who
  expected $50 leaves within a month and drives up churn, which CIP-005 pays
  for. *Mitigation:* an earnings estimator in onboarding, before install.
- **CoinPay payout failures.** *Mitigation:* accrue locally, retry, expose
  pending balance; never silently drop a period.
- **Repair remains unpaid and unrewarded.** Carried from CIP-005; the open
  question about partial repair funding should be settled in this CIP since
  this is where the money is defined.

## Estimate

**4–6 weeks.** ~1 week challenge issuance and verification, 1 week metering and
receipts, 1 week payout computation and CoinPay integration, 1 week customer
billing and reconciliation, 0.5 week the earnings CLI, 1 week fraud controls and
adversarial tests.

## Open questions

- Should repair egress be partially paid (e.g. 25% of the egress rate) to fix
  CIP-005's incentive gap? Costed at 5% churn this is ~$0.00035/GB-month against
  a $0.0014 gross margin — affordable at a quarter rate, not at full.
- Payout cadence: monthly is simple, but weekly would reduce provider anxiety
  early on. CoinPay transaction costs decide this.
- Does the $0-internal-egress promise need a fair-use ceiling? Provider egress
  payouts on internal reads have no offsetting revenue.
- Minimum challenge sample: one shard per block per round, or every shard?
  Cheaper sampling means a dishonest node survives longer.
