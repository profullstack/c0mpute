# Scheduling

**Reference implementation**, not a protocol requirement.

This page describes how *our* client picks a winner. Another
implementation may weigh offers completely differently and still be a
correct c0mpute node — that is the point. What the protocol fixes is that
**somebody on the buyer's side decides**, and that whoever decides can
prove afterwards why.

There is no globally authoritative matcher. The decision is made by one
of:

- the buyer's local client;
- a managed gateway the buyer explicitly delegated to;
- a private organization scheduler on a private overlay.

## Eligibility is not scoring

Two different questions, deliberately separated:

| | |
|---|---|
| **Eligibility** | a hard filter. Failing it is disqualifying, not merely unattractive. |
| **Scoring** | a preference. Reasonable buyers disagree, and all of them are right. |

An implementation that gets scoring "wrong" makes bad purchases. An
implementation that gets eligibility wrong pays for work that cannot
happen. Only the second is a correctness bug, so only the second is
specified tightly.

### The eligibility gates

A provider must clear all of these before its price is ever looked at:

1. **Advert verifies and is unexpired.** Signature checks out and
   `expiresAt` has not passed.
2. **Workload offered.** The provider advertises this job's workload
   namespace.
3. **Allowlist.** If the job carries one, the provider is on it. Checked
   *before* the trust tier — "one of these identities" is a stronger
   statement than any tier a provider can claim for itself.
4. **Trust tier.** The provider's claimed tier satisfies the job's.
5. **Region.** If the job constrains region, the provider disclosed one
   and it matches. An undisclosed region never matches a constraint.
6. **Hardware.** GPU count, VRAM, GPU features, CPU cores, memory.
7. **Offer validity.** Right job, within `maxPrice`, and able to finish
   before `deadline`.

**VRAM does not sum across cards.** Two 12 GiB GPUs do not satisfy a
24 GiB requirement, because a model that does not fit on one card does not
fit on two by addition. Counts *do* sum, across cards that each
individually meet the spec.

**Price is compared exactly**, as decimal-string arithmetic. Lexically
`"0.042"` sorts above `"0.10"`, and `"0.9"` sorts below it — a string
comparison rejects good offers and accepts ones that blow the cap.
Floating point appears only in scoring, where a rounding error changes a
preference rather than a correctness check.

### Explaining an empty market

"No providers found" is the most common thing a buyer sees on a young
network, and it is useless on its own. Eligibility checks return *every*
failed requirement rather than the first, so a client can say which
constraint is actually binding — nobody has 24 GiB of VRAM, or six
providers do but none disclosed a region — instead of making the buyer
relax constraints one at a time.

## One offer per provider

The offer book keeps a provider's **best** offer, not its most recent:
cheaper wins, then faster, then lower offer hash.

Keeping the best rather than the latest makes the book independent of
arrival order, so two buyers who heard the same offers in different
sequences select the same winner. A provider improving its quote is
legitimate; a provider *worsening* it after the fact should not be able to
withdraw the better price it already signed.

## Policies

```bash
c0mpute run job.json --policy cheapest
c0mpute run job.json --policy fastest
c0mpute run job.json --policy balanced   # default
c0mpute run job.json --policy trusted
c0mpute run job.json --policy private
```

Weights over four dimensions, each normalized to `0.0..=1.0`:

| policy | price | speed | reputation | trust |
|---|---|---|---|---|
| `cheapest` | 1.0 | — | — | — |
| `balanced` *(default)* | 0.4 | 0.2 | 0.4 | — |
| `fastest` | — | 1.0 | — | — |
| `trusted` | 0.2 | — | 0.5 | 0.3 |
| `private` | 0.2 | — | 0.3 | 0.5 |

`private` additionally **requires** the job to carry a provider
allowlist. Without one, "private" would be a preference rather than a
guarantee, and a caller asking for it almost certainly meant otherwise.

### Why `balanced` is the default

Pure lowest-bid selection is how a market races to the bottom. The
cheapest quote often comes from the provider most likely to vanish
mid-job, and the buyer pays for that in retries and missed deadlines.
Pricing reputation alongside money means a provider that finishes what it
starts can charge slightly more and still win, which is the incentive the
network wants to create.

### Proportional scoring, not min–max

Lower-is-better dimensions score as `best / value`, **not** as min–max
normalization across the candidate set.

Min–max stretches whatever spread happens to be present across the full
range, so with two offers a one-cent gap and a hundred-dollar gap both
produce a 0.0-versus-1.0 split. Any price difference, however trivial,
then dominates every other dimension — a provider that fails most of its
jobs wins on being a cent cheaper, which is precisely what `balanced`
exists to prevent.

A ratio is proportional and scale-invariant: an offer 20% dearer than the
best scores 0.83, whether the amounts are cents or thousands.

### Determinism

Two nodes running the **same** policy over the **same** offers must reach
the same answer. So:

- eligible providers and live offers are returned in DID order, never
  hash-map order;
- ties in score break by provider DID;
- the offer book's best-offer rule removes arrival-order sensitivity.

Two nodes running *different* policies reaching different answers is not a
bug. It is the design.

## Reputation

Derived locally from signed receipts — see
[receipts.md](receipts.md#deriving-reputation). Three properties that
matter to scheduling:

**A new provider is not a bad provider.** Success rate is smoothed toward
a neutral prior (Laplace, one imaginary success and one imaginary
failure), so an unknown provider scores exactly 0.5. Raw rates would make
a provider's first job either unwinnable at 0/0, or make a fresh key with
one success outrank a provider with a thousand jobs and two failures.

**Failures count, which is why they are signed.** A network that only
signs successes has a reputation system that measures nothing. A disputed
job costs more than a failed one — a failure is a bad day, a dispute is a
disagreement about whether the work happened at all.

**Quote accuracy is hard to fake.** The quoted duration was signed before
the work and the actual duration after it, and the receipt cites the offer
by hash. A provider that systematically under-quotes leaves a signed trail
of doing so. Crediting accuracy requires holding both the offer and a
receipt that names it, so a flattering pair cannot be assembled after the
fact.

## What an intermediary can and cannot do

Every input is verified at the boundary, so an indexer, relay or gateway
sitting in the path can:

- **delay or withhold** records — you see a smaller market and may get a
  worse price;

and cannot:

- forge an advert, inflate a provider's capabilities, undercut a quote,
  re-point an offer at a different job, or manufacture reputation.

Degraded, not corrupted, is the correct failure mode for a
non-authoritative index — and the reason a buyer who suspects one can go
to the network directly.

## Reference

`node/crates/c0mpute-market`. The full chain — advert, job, offer,
selection, receipt, countersignature — runs end to end with no shared
state between the parties in that crate's `tests/gateway_none.rs`,
including the hostile-intermediary cases above.
