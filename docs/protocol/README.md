# The c0mpute protocol

**Status:** Phase 1 complete, Phase 2 in progress. Envelopes, identity,
the four message types, buyer-side scheduling and receipt-derived
reputation are specified and implemented. The **transport** is not:
nothing yet carries adverts and offers between machines. Discovery,
relays, settlement adapters and the validation tiers are named here but
not yet written — the pages that do not exist yet are listed at the bottom
rather than stubbed, so nothing in this directory describes code that has
not been built.

> c0mpute.com infrastructure is not required for the c0mpute network to
> operate.

That sentence is the point of this directory. Everything below is designed
so that two peers who have never heard of us can find each other, agree on
a price, run work, check the result and settle — and so that anyone
holding the resulting records can verify them without asking us, or
anyone, for permission.

## Read this first: four kinds of thing

Every component of c0mpute is exactly one of these, and mixing them up is
how a decentralized network quietly acquires a single point of failure
(DIP-0024):

| | meaning |
|---|---|
| **protocol requirement** | an implementation MUST do this to interoperate |
| **reference implementation** | how *we* do it; another node may differ |
| **optional hosted service** | an accelerator; the network works without it |
| **first-party integration** | our product, best-supported, never mandatory |

Indexers, gateways, dashboards, relays and the Infernet control plane are
all in the third column. They may cache, observe, proxy and bill. None of
them is authoritative: none may be *required* for two honest peers to
transact.

## The message types

Four signed records carry the market. Each is self-contained — everything
needed to verify it is inside it.

```text
c0mpute.provider.advert/v1    a provider says what it can run, and until when
c0mpute.job/v2                a buyer describes work, constraints, price, trust
c0mpute.offer/v1              a provider quotes a binding price for one job
c0mpute.receipt/v1            the provider signs what actually happened
c0mpute.receipt.acceptance/v1 the buyer countersigns, or disputes
```

They chain by content hash, so no participant has to be trusted to report
the previous step honestly:

```text
  provider.advert ──┐
                    ├──> offer ──> receipt ──> acceptance
  job ──────────────┘      │          │            │
   │                       │          │            │
   └── job id ─────────────┴──────────┘            │
       (hash of the manifest)                      │
                          offer hash ──────────────┘
                                     receipt hash
```

An offer names the job it bids on *and* the advert it is backed by. A
receipt names the job and the offer that priced it. An acceptance names
the receipt. Every arrow is a hash, so a third party holding the JSON can
check that the price paid is the price quoted, and that the quote came
from a provider that claimed the capabilities relied on.

## Pages

| page | what it fixes |
|---|---|
| [canonical-json.md](canonical-json.md) | the exact bytes a signature covers |
| [identity.md](identity.md) | `did:c0mpute:z…`, and why identity is not payment |
| [envelopes.md](envelopes.md) | the signed wrapper, and domain separation |
| [providers.md](providers.md) | `provider.advert/v1` |
| [jobs.md](jobs.md) | `job/v2` |
| [offers.md](offers.md) | `offer/v1` |
| [receipts.md](receipts.md) | `receipt/v1`, acceptance, and derived reputation |
| [scheduling.md](scheduling.md) | eligibility, offer selection, and reputation scoring |

## Implementation

`node/crates/c0mpute-envelope` is the reference implementation of the
message types; `node/crates/c0mpute-market` is the reference
implementation of the buyer side — provider directory, offer book,
reputation ledger and selection.

Neither depends on a transport, an HTTP client or a settlement product.
Identity enters as raw ed25519 bytes, settlement is named by an open
string, and time enters as a parameter rather than being read from the
clock. That is what makes DIP-0024's dependency rule checkable rather than
aspirational — and what makes every expiry path testable rather than a
matter of waiting.

Cross-implementation test vectors are pinned in `c0mpute-envelope`'s
`tests/vectors.rs`. A second implementation is correct when it reproduces
those DIDs and hashes exactly. To print them:

```sh
cargo test -p c0mpute-envelope --test vectors -- --nocapture print_vectors
```

The full chain — advert, job, offer, selection, receipt, countersignature
— runs end to end with no shared state between the parties in
`c0mpute-market`'s `tests/gateway_none.rs`, which also covers what a
hostile intermediary can and cannot do.

## Not yet written

These are real parts of the v2 direction with no specification here,
because there is no implementation to specify:

- **discovery** — DHT provider lookup and gossip topic layout (Phase 2)
- **relays** — NAT traversal and outbound-only providers (Phase 2)
- **settlement** — what a settlement adapter must do (Phase 3)
- **validation** — the L0–L5 tiers as a wire contract (Phase 3)

`ValidationPolicy` and `SettlementAdapter` already travel in `job/v2` and
`receipt/v1`, so a job can *state* its validation level and settlement
rail today. What an implementation must do to honour either is the part
still missing.

Scheduling and receipt-derived reputation moved off this list with
`c0mpute-market` — see [scheduling.md](scheduling.md). What is still
missing there is the **transport**: the market logic runs, and nothing yet
carries adverts and offers between machines.
