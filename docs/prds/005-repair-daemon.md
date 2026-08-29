---
cip: 005
title: "Auto-repair daemon"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-08-29
updated: 2026-08-29
implements: DIP-0012 (0012-storage-plugin.md) Phase 4
depends-on: 003, 004
blocks:
implementation:
estimate: "3–4 weeks"
---

## Summary

Continuously detect blocks that have lost shards and regenerate them onto fresh
peers before loss becomes unrecoverable. CIP-001 established that our cost
advantage is bought by spending Storj's durability margin, which means repair
speed is not an operational nicety — it is the component that makes RS 10/14 a
defensible choice instead of a reckless one.

## Motivation

RS 10/14 tolerates 4 lost shards. At 5% monthly node churn, a block loses its
first shard within weeks and its fourth within a few months. Without repair,
every object in the network trends toward unrecoverable on a predictable
timetable. There is no version of this program where repair is optional or
deferred.

Compare the two designs honestly, from CIP-001's table:

| | c0mpute RS 10/14 | Storj RS 29/80 |
|---|---|---|
| Tolerates | 4 losses | 51 losses |
| Durability @ p=0.95 | 3.4 nines | 46.8 nines |
| Repair urgency | **hours** | weeks |
| Storage cost | 1.4x | 2.76x |

We chose the left column. The bill for that choice is paid here.

## Goals

- Detect shard loss within one scan interval (target: 1 hour).
- Repair a degraded block to full `n` within 6 hours of detection.
- Never let repair traffic starve customer reads.
- Repair without needing the customer's client to be online.
- Prove repair happened, so providers can be paid and reputations adjusted.

## Non-goals

- Proving a node *currently holds* a shard it claims to (CIP-006 challenges).
  This CIP trusts `Have` responses; a lying node is a CIP-006 problem.
- Repairing data whose root pointer is lost — unreachable is not degraded, it
  is garbage, and CIP-004's GC handles it.

## Design

### Who repairs?

Not the customer's client: a laptop that is closed for a week cannot be the
thing standing between the network and data loss.

Repair is performed by **shard-holding nodes acting for the blocks they
already participate in.** For each block a node holds a shard of, it is a
candidate repairer. To avoid 14 nodes all repairing the same block
simultaneously, the repairer is deterministic:

```
repairer_for(block, round) = argmin over healthy holders h of
                             blake3(block_hash || round || h.peer_id)
```

Every holder computes the same answer without coordination. If the elected
repairer is itself gone, the next round elects someone else. This is rendezvous
hashing, and it needs no consensus — which matters under DIP-0011.

### Detection

Each storage node scans the blocks it participates in, on a rolling schedule
sized so every block is checked once per `scan_interval` (default 1 hour):

1. Batch `Have` probes (CIP-003's batched existence probe) to the other `n-1`
   holders.
2. Count healthy shards.
3. Classify:

| Healthy shards (of 14) | State | Action |
|---|---|---|
| 14 | `healthy` | none |
| 12–13 | `degraded` | repair, normal priority |
| 11 | `urgent` | repair, high priority, preempt background work |
| ≤10 | `critical` | repair immediately; alert; block is one loss from death |
| <10 | `lost` | cannot repair; record and surface loudly |

Thresholds are `(k, n)`-relative, not absolute: repair triggers at
`n - floor(parity/2)` healthy, i.e. as soon as half the parity budget is spent.
Waiting until `k+1` would be cheaper and is what Storj's margin lets them do;
we cannot afford it.

### Repair

To repair block `B` missing shards `{i, j}`:

1. Acquire a repair lease on `B` via gossip (`c0mpute/storage/repair/v1`),
   naming the repairer and an expiry. Duplicate work is wasteful, not
   incorrect — the lease is an optimisation, and a lost lease race just means
   two nodes repair the same block.
2. Fetch `k` shards from healthy holders, streaming (CIP-003).
3. RS-reconstruct, verify against the block hash in the manifest.
4. Regenerate only the missing shards `{i, j}` — not all `n`.
5. Select replacement peers under CIP-003's diversity constraints, **excluding
   every peer already holding a shard of this block**.
6. `Put` the regenerated shards.
7. Publish a signed repair attestation (see below) and update `host_hint` in
   the manifest, which requires advancing the volume root (CIP-004).

Step 7 is the awkward one: repair mutates metadata the customer owns. The
repairer cannot sign the customer's root. Resolution: `host_hint` is
explicitly a *hint*, and the DHT is the source of truth for shard location
(CIP-003). Repair updates DHT provider records, which needs no customer
signature, and the stale hint is corrected opportunistically the next time the
customer's client writes. **Reads never depend on hints being fresh.**

### Repair attestations

```json
{
  "block": "blake3:...",
  "object": "blake3:...",
  "repairer": "did:coinpay:...",
  "round": 41,
  "shards_regenerated": [3, 11],
  "sources": ["did:...", "..."],
  "bytes_read": 4194304,
  "completed_at_ms": 1756512000000,
  "signature": "..."
}
```

Published to gossip and retained by holders. Three uses: proving repair for
provider reputation, attributing repair bandwidth (unpaid but measured, per
CIP-001), and detecting nodes that repeatedly fail to serve repair reads.

### Not starving customers

Repair is bulk background traffic competing with latency-sensitive reads on
consumer uplinks. Controls:

- A per-node repair bandwidth budget, default **20% of measured uplink**,
  configurable, enforced by a token bucket on repair streams.
- Repair reads are marked low-priority; a node under customer read load sheds
  repair first.
- `urgent`/`critical` blocks bypass the budget — data loss beats latency.
- Global backpressure: if a node's repair queue exceeds a threshold, it stops
  accepting *new* shard placements. A node that cannot keep its existing data
  healthy has no business taking more.

### The churn storm

The failure mode that kills p2p storage networks: a large operator leaves,
mass repair starts, repair traffic saturates uplinks, healthy nodes time out
and are misclassified as failed, which triggers more repair. Reflexive
collapse.

Defences:

- **Distinguish unreachable from gone.** A shard is only presumed lost after
  `grace_probes` (default 6) failures spread over `grace_window` (default 2
  hours). Brief outages must not trigger repair; most consumer nodes flap.
- **Announced departures drain gracefully.** A node running
  `c0mpute storage retire` announces, keeps serving while its shards are
  re-placed, and exits clean with reputation intact. Make the good path
  attractive so it is the common one.
- **Network-wide repair rate limit.** If more than `X%` (default 5) of blocks
  are simultaneously degraded, the network is in a storm: repair proceeds
  strictly in priority order (`critical` first) at a capped global rate rather
  than everyone repairing everything at once.
- **Never repair onto a node that is itself draining.**

## Acceptance criteria

1. On a 20-node testnet, killing 2 of 14 holders of a block results in a fully
   repaired block within one scan interval, on fresh peers satisfying diversity.
2. Exactly one node performs the repair (rendezvous election verified in logs);
   under an induced lease race, at most 2 do, and the result is still correct.
3. Repair regenerates only missing shards — the 12 survivors are byte-identical
   before and after.
4. A node offline for 30 minutes and back does **not** trigger repair of its
   shards.
5. Under a synthetic 100 Mbit uplink cap, customer read p99 rises less than 20%
   while repair runs at its budget.
6. Retiring a node with `retire` re-places all its shards with zero degraded
   blocks at any point.
7. Simulated 30%-of-network departure: no cascading failure; repairs complete
   in priority order; no block reaches `lost`.
8. Repair attestations verify against the repairer's DID.

## Risks

- **Repair is unpaid, so nodes are incentivised to skip it.** A node that never
  repairs saves bandwidth and loses nothing directly. *Mitigation:* attestations
  feed reputation; low-reputation nodes stop receiving placements and therefore
  stop earning. Make repair participation a condition of the storage role.
  This deserves its own scrutiny in CIP-006 — it is the sharpest incentive
  misalignment in the design.
- **Scan cost grows with data held.** A node with 10 TB and 4 MiB blocks tracks
  ~2.5M blocks; probing all hourly is a lot of messages. *Mitigation:* batched
  `Have` probes; scan at the *object* level with per-block detail only on
  suspicion; scale block size with object size (CIP-002/004) which cuts block
  count by orders of magnitude on large objects.
- **Repair storms hitting CoinPay.** *Mitigation:* repair touches DHT records,
  not roots — deliberately, per the design above.
- **The 20% bandwidth budget is wrong for real uplinks.** *Mitigation:* it is
  configurable; measure across a real fleet before defaulting anything.

## Estimate

**3–4 weeks.** ~0.5 week detection and classification, 0.5 week rendezvous
election and leases, 1 week the repair path, 0.5 week attestations, 1 week
bandwidth control and storm defences, 0.5 week the chaos test harness.

## Open questions

- Should repair be *paid* after all, funded from the storage margin, to fix the
  incentive problem at its root rather than via reputation? CIP-001's cost model
  says a paid repair egress at 5% churn costs $0.0014/GB-month against a
  $0.0014/GB-month gross margin — so paying full rate is impossible, but paying
  a fraction may be affordable and worth it.
- Is one hour the right scan interval? It sets worst-case exposure directly.
- Should `critical` blocks trigger a temporary tier upgrade (extra parity)
  until the network is healthy again?
