---
cip: 001
title: "Storage program: durability model, tiers, and economics"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-08-29
updated: 2026-08-29
implements: DIP-0012 (0012-storage-plugin.md)
depends-on:
blocks: 002, 003, 004, 005, 006, 007
implementation:
estimate: "1 week (analysis + simulation, no production code)"
---

## Summary

This CIP sets the parameters every other storage CIP builds on: which
redundancy scheme backs which tier, what we pay providers, what we charge
customers, and what durability we can honestly claim. It produces no shipping
code — it produces numbers that the rest of the program is not allowed to
contradict.

The headline decision: **Reed-Solomon 10/14 is the default, replication is a
tier not a default, and the retail price is $0.0035/GB-month** — which
undercuts Storj, the only p2p network that is actually comparable on
retrieval latency.

## Resolve the DIP-0012 collision first

The repo currently contains **two files numbered DIP-0012, both marked
`Accepted`, both dated 2026-05-03, asserting opposite things**:

- `dips/0012-no-storage-network.md` — "c0mpute is compute-only; we don't run a
  storage network."
- `dips/0012-storage-plugin.md` — "c0mpute hosts files: Reed-Solomon 10/14."

Neither declares `supersedes` or `superseded-by`. This is a live hazard: the
next contributor to read `dips/` in lexical order finds the wrong one first.

**Action, in the PR that lands this CIP:** mark `0012-no-storage-network.md`
as `Superseded`, point its `superseded-by` at the storage-plugin DIP, and fix
the DIP index table which currently reads "c0mpute is compute-only; storage is
BYOS" for 0012. The storage-plugin DIP's own motivation table already records
that this position was drafted and withdrawn — the file just never got its
status updated.

## Goals

- Pick `(k, n)` per tier, with the durability arithmetic written down.
- Set provider payout rates and customer retail prices that leave a real
  margin, including repair traffic.
- State a defensible "cheapest" claim that survives someone checking it.
- Define what a node must promise before it is allowed to hold shards.

## Non-goals

- Cold/archival tier. We are structurally bad at it (see
  `docs/storage-pricing-scenarios.md` scenario 4) and Glacier Deep Archive at
  $0.00099/GB is not a fight worth having. Revisit only if a customer pays.
- Token, staking, or collateral design. Payouts ride CoinPay (DIP-0007);
  slashing is reputation-based, not bonded.

## Design

### Why erasure coding, not 3-copy replication

The instinct to "keep 3 copies" is the single most expensive decision
available here, because the redundancy factor *is* the cost of goods:

| Scheme | Raw GB per usable GB | Tolerates | Repair amplification |
|---|---|---|---|
| 3-copy replication | **3.00x** | 2 losses | **1x** (copy a survivor) |
| RS 10/14 (shipped) | **1.40x** | 4 losses | **10x** (fetch k shards) |
| RS 20/32 | 1.60x | 12 losses | 20x |
| Storj RS 29/80 | 2.76x | 51 losses | 29x |

Replication and erasure coding trade the same two costs in opposite
directions: **replication is cheap to repair and expensive to store; erasure
coding is cheap to store and expensive to repair.** Everything below follows
from that sentence.

Against Storj specifically, our expansion factor is the structural advantage.
At an identical provider payout rate, RS 10/14 costs **1.97x less** per usable
GB than RS 29/80. That is not a cleverness advantage that Storj can copy back
— widening their code would cut their own durability margin, which is what
their slow-repair architecture spends it on.

### Durability is bounded by per-node availability, not by parity

This is the part that is easy to get wrong. RS 10/14 needs 10 of 14 shards. If
each shard host is independently available with probability `p`, an object is
readable with probability `P(X >= 10)` where `X ~ Binomial(14, p)`:

Computed by `scripts/storage-durability-sim.py`:

| Per-node availability | RS 10/14 | 3-copy | RS 20/32 | Storj RS 29/80 |
|---|---|---|---|---|
| 0.90 | 9.2e-3 (2.0 nines) | 1.0e-3 (3.0) | 5.5e-6 (5.3) | 1.6e-32 (31.8) |
| 0.95 | 4.3e-4 (3.4 nines) | 1.3e-4 (3.9) | 1.7e-9 (8.8) | 1.6e-47 (46.8) |
| 0.99 | 1.9e-7 (6.7 nines) | 1.0e-6 (6.0) | 2.9e-18 (17.5) | 2.2e-83 (82.7) |
| 0.999 | 2.0e-12 (11.7 nines) | 1.0e-9 (9.0) | 3.4e-31 (30.5) | 2.8e-135 (134.5) |

**RS 10/14 on 95%-available consumer nodes yields roughly three nines, not
eleven.** The "11 nines" figure in `docs/storage-pricing.csv` is only reachable
with ~99.9% per-node availability *and* fast repair.

Two further readings of that table are worth internalising, because both cut
against intuition:

- **At low availability, 3-copy beats RS 10/14** (3.9 nines vs 3.4 at p=0.95).
  Needing 1-of-3 is a weaker demand than 10-of-14. Wide erasure codes only pull
  ahead once nodes are individually reliable. This is another argument for the
  `hot` tier being replicated rather than coded.
- **Storj's 29/80 is not waste, it is a different bet.** 46 nines at p=0.95
  means they can repair *lazily* — batch it, run it cheaply, tolerate a node
  being gone for weeks. Our 1.4x expansion buys the cost advantage by spending
  their safety margin, which means **we are obligated to repair fast.** CIP-005
  is therefore not a nice-to-have that follows the launch; it is the load-bearing
  component of this entire cost model. If repair is slow or broken, RS 10/14 is
  the wrong code and we will lose data that Storj would not have lost.

Two consequences for implementation:

1. **Placement must be reputation-gated.** Shards go only to nodes with
   `reputation >= 0.9` (`c0mpute-verify::reputation`) and 30-day uptime
   `>= 99%`. A node below that line can still run compute; it just doesn't get
   paid to hold data. CIP-003 enforces this.
2. **`(k, n)` must be configurable, not hard-coded.** `c0mpute-store::erasure`
   already takes `k` and `parity` as arguments and only the *defaults* are
   10/14 — good. `ObjectManifest` already persists `k` and `parity` per object,
   so a future parameter change is not a migration. Keep it that way.

Correlated failure is not modelled above and is the thing most likely to bite:
14 shards behind one ISP, one power grid, or one hosting provider are not 14
independent samples. CIP-003 requires ASN and region diversity in placement for
this reason, and the availability figures should be read as an upper bound
until CIP-005's repair loop is measured in production.

### Tiers

Three tiers, matching DIP-0012 v3, with the redundancy scheme now pinned:

| Tier | Scheme | Expansion | Best for | Retail $/GB-mo |
|---|---|---|---|---|
| `hot` | 3-copy replication | 3.0x | Small files, metadata, DB-adjacent, latency-sensitive reads | **$0.006** |
| `standard` | RS 10/14 | 1.4x | Default. Bulk data, media, datasets | **$0.0035** |
| `critical` | RS 20/32 | 1.6x | Irreplaceable data, long retention | **$0.005** |

`hot` exists precisely because replication's 1x repair amplification and
single-peer reads make it the right tool for small, frequently-read, frequently-
rewritten objects — which is exactly what a filesystem's metadata and a
database's pages look like. CIP-007 places filesystem metadata on `hot` for
this reason. So the three-copy instinct was right, just for a narrower job than
"everything".

Encryption is orthogonal to tier and is covered by CIP-011; every tier can be
client-encrypted.

### Provider economics

Providers are paid per raw GB actually held, per month, prorated hourly, plus
per GB served to paying customers.

| Line item | Rate | Notes |
|---|---|---|
| Storage held | **$0.0015 / raw GB / month** | Matches Storj's node rate, so no reason to prefer them |
| Customer egress served | **$0.002 / GB** | Paid only for bytes a customer actually pulled |
| Repair egress | **$0 — unpaid obligation** | See below. This is load-bearing. |
| Ingress | $0 | Writes are free to accept |

**Repair egress must be unpaid, or the pricing collapses.** Work the numbers:
at monthly node churn `c`, the bytes that must be *read* to rebuild what was
lost, per usable GB, is `k × expansion × c` = `10 × 1.4 × c`. At 5% monthly
churn that is 0.7 GB of transfer per usable GB per month. Paying the egress
rate on it would add $0.0014/GB-month to a cost base of $0.0021 — a 67% COGS
increase that erases the margin outright.

Storj resolves this the same way: nodes are paid for customer egress, not for
repair traffic. So this is standard, not sharp practice — but it must be
**explicit in the provider terms**, because it means a node with a metered or
tightly capped residential uplink is a bad fit and will feel cheated later.
Provider onboarding (CIP-006) states an uplink expectation up front.

### Customer economics and the "cheapest" claim

COGS per usable GB-month, at the $0.0015 provider rate:

| Tier | Expansion | COGS | Retail | Gross margin |
|---|---|---|---|---|
| `standard` (RS 10/14) | 1.4x | $0.0021 | $0.0035 | **40%** |
| `hot` (3-copy) | 3.0x | $0.0045 | $0.006 | 25% |
| `critical` (RS 20/32) | 1.6x | $0.0024 | $0.005 | 52% |

Egress: **$0 internal** (to c0mpute jobs — the compute-locality argument that
DIP-0012 rests on), **$0.004/GB to the public internet**, undercutting Storj's
$0.007.

Now the honest framing of "cheapest of any p2p storage", because the claim as
stated does not survive contact with the price sheet:

| Network | $/GB-mo | Why it is or isn't comparable |
|---|---|---|
| Filecoin | $0.0001 | Archival. Deal minimums, 180-day terms, retrieval in minutes-to-hours |
| Lighthouse | $0.0003 | One-time payment, Filecoin-backed, no mutable access |
| Sia | $0.001 | 3-month renewal commitment; tiny network; volatile |
| **c0mpute `standard`** | **$0.0035** | Hot, mountable, read/write, no commitment |
| Storj DCS | $0.004 | Hot, S3, no commitment — **the real comparable** |
| Crust | $0.001 | Small network, native API only |

**We cannot be cheaper than Filecoin or Sia, and should never claim to be.**
They are cold or commitment-bound; a POSIX read/write mount over them is not a
product anyone can use. The claim that is both true and marketable:

> The cheapest p2p storage you can actually mount — no commitment, no
> minimums, no token, and $0 egress to compute.

Against Storj — the only network offering comparable retrieval latency and no
commitment — we are **12.5% cheaper on storage and 43% cheaper on egress**, and
the margin to do it comes from the expansion-factor advantage, not from
underpaying providers.

### What a storage node must promise

Enforced by CIP-003 placement and CIP-006 challenges:

- 30-day uptime >= 99%, reputation >= 0.9
- Unmetered or high-cap uplink; repair traffic is unpaid
- Minimum 100 GB committed, minimum 30-day intent
- Responds to byte-range challenges within 5s, >= 99% pass rate
- Graceful exit: announce, let repair drain your shards, then leave. Nodes that
  vanish without announcing take a reputation hit that gates them out of
  placement.

## Acceptance criteria

1. `dips/0012-no-storage-network.md` has `status: Superseded` and
   `superseded-by: DIP-0012 (0012-storage-plugin.md)`; the DIP index row for
   0012 reads the storage-plugin title.
2. A committed simulation (`scripts/storage-durability-sim.*`) reproduces the
   availability table above and is re-runnable with different `(k, n, p)`.
3. `docs/storage-pricing.csv` rows for c0mpute are updated to the tier prices
   here, and gain a `tier` column.
4. `docs/storage-pricing-scenarios.md` is re-run against the new prices.
5. No other CIP in this program cites a price or `(k, n)` not listed here.

## Risks

- **Consumer-node availability comes in under 99%.** Then `standard` delivers
  ~3 nines and the durability claim has to be restated or `(k, n)` widened.
  *Mitigation:* the sim is parameterised; gate placement on measured uptime
  from day one rather than assuming it.
- **Churn is much higher than 5%/month.** Repair traffic grows linearly and
  providers with capped uplinks quit, which raises churn further. This is the
  one genuinely reflexive failure mode in the design. *Mitigation:* measure
  churn before public launch; if it exceeds 10%/month, move `standard` to
  RS 20/32 and reprice.
- **Storj cuts prices in response.** Their expansion factor means they'd be
  cutting into node pay to do it. *Mitigation:* none needed; the structural
  advantage is real, but don't build a plan that requires them to stand still.
- **Nobody supplies disk at $0.0015/GB.** *Mitigation:* the rate matches an
  existing market clearing price; if supply is short, raise payout and margin
  absorbs it down to ~$0.0021 before `standard` goes underwater.

## Estimate

**1 week.** Analysis, the durability simulation, and the pricing doc updates.
No production code. Do not start CIP-002 before this is Approved — every later
phase hard-codes numbers from this document.

## Open questions

- Does `hot` (3-copy) need its own placement policy, or does CIP-003's
  reputation gate suffice at n=3? Losing 2 of 3 is far likelier than losing 5
  of 14.
- Should `critical` be RS 20/32 or RS 16/24? 20/32 doubles read fan-out versus
  `standard`, which may hurt more than the durability helps.
- Minimum billable object size. At 4 KiB, 14 shards of ~300 bytes each plus a
  manifest is mostly overhead — the classic small-file problem. CIP-007 packs
  small files into larger blocks, but the *billing* floor still needs a number.
