---
dip: 0024
title: "Separate the protocol from every hosted service that accelerates it"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-09-07
updated: 2026-09-07
discussion:
implementation: node/crates/c0mpute-envelope (DIP-0025); `--gateway none` CI gate pending Phase 2
supersedes:
superseded-by:
---

## Summary

Every c0mpute component is exactly one of four things, and which one it is
must be written down where the component lives:

```text
protocol requirement       an implementation MUST do this to interoperate
reference implementation   how *we* do it; another node may differ
optional hosted service    an accelerator; the network works without it
first-party integration    our product, best-supported, never mandatory
```

Optional hosted services — indexers, gateways, dashboards, relays, fiat
billing, the Infernet control plane — may cache, observe, proxy, bill for,
and speed up the network. None of them may be **authoritative**: required
in order for two honest peers to discover each other, agree on a price,
execute work, verify a result, or produce a receipt.

The rule this DIP exists to protect:

> If c0mpute.com disappears, c0mpute keeps computing.

## Motivation

DIP-0011 already says c0mpute has no central application backend, and for
c0mpute proper that has held: there is no Postgres, no Supabase, no
coordinator daemon. But "no central backend" was asserted as a deletion,
not as a property anything checks, and two things have drifted since.

**Infernet still has an authoritative control plane.** Provider census,
routing, heartbeats, job state, and distributed-inference coordination run
through a hosted service. It works, and it is the reason Infernet shipped;
it is also a single point at which the network can be switched off, and
today nothing in the repository would fail if that dependency deepened.

**The v1 market types rely on transport authenticity.** `CapabilityAd`,
`JobOffer`, `JobBid`, `JobAccept` and `JobReceipt` in
`c0mpute-net::topics` carry no signatures of their own — gossipsub signs
each message with the publisher's key, and that is the whole of their
provenance. A gossipsub signature dies at the first hop, so an advert
relayed by an indexer or a receipt read back from disk is unverifiable.
Reputation built on unverifiable receipts has to be believed on some
operator's word, which means it has to live in some operator's database.
The decentralization is real at the transport layer and absent at the
record layer.

Neither is a hosted service being *bad*. Managed gateways, indexers and
fiat billing are genuinely useful, and a commercial layer is the plan
(v2 direction §20). The problem is that "useful" and "load-bearing" look
identical in a codebase until something breaks, and by then the dependency
is everywhere.

## Detailed design

### The classification is written down

Every architectural document, crate, and service README states which of
the four categories it is in. Not a header for its own sake: it is the
question you have to answer before adding a dependency, and writing it
down is what makes the answer reviewable.

### Core protocol crates may not depend on hosted-service SDKs

Enforced in CI, not by convention:

```text
core protocol crates MUST NOT depend on:
  Supabase SDKs
  Railway-specific APIs
  c0mpute.com hosted endpoints
  managed billing implementations
```

The current core set is `c0mpute-envelope`, `c0mpute-proto`,
`c0mpute-net`. `c0mpute-envelope` is already built to the rule: no libp2p,
no HTTP client, no settlement implementation. Identity enters as raw
ed25519 bytes; settlement is named by an open string rather than a closed
enum, so CoinPay is the default rather than a requirement.

A dependency check is cheap to write and catches the drift on the commit
that introduces it, rather than at the release where someone finally asks
whether the CLI works offline.

### Hosted services are clients of the public protocol

A managed gateway submits jobs, collects offers and reads receipts through
the same messages any node uses. There is no privileged internal
scheduling API, and no message type that only our infrastructure can
produce. If a gateway needs a capability the protocol lacks, the fix is a
protocol change everyone gets, not a private endpoint.

This costs us something real — a private API would be faster to build —
and it buys the property that a third-party gateway is as capable as ours,
which is the difference between a protocol and a product with an SDK.

### `--gateway none` is the release gate

The claim "you do not need us" is only true if it is tested. Phase 2 adds
`--gateway none` as a release-gating integration test: a job discovered,
offered on, awarded, executed, validated and receipted with c0mpute.com,
its APIs, its databases and the Infernet control plane all unreachable.

Until that test exists and passes, this DIP is Draft and the website
should not claim the property.

### Indexers are a view, never a source

An indexer may cache adverts, materialize the receipt stream into SQL, and
serve fast queries. Because every record it serves is independently
signed, an indexer can be stale, partial, or lying about *which* records
exist — and cannot forge one. Two indexers disagreeing is a normal
condition, not an incident; a buyer that doubts both queries the network
directly. Anything presented as network data is labelled an observed view.

## Alternatives considered

**Keep the control plane authoritative and market the network as
decentralized.** Cheapest, and it is where the project already is. It
fails the moment anyone checks, and "decentralized" claims that do not
survive inspection are worse for the brand than not making them.

**Go the other way: forbid hosted services entirely.** Ideologically
tidy, commercially fatal, and worse for users. Onboarding, fiat billing,
SLAs and observability are real needs; a protocol that cannot be wrapped
in a managed service just gets wrapped in someone else's.

**Rely on review discipline rather than a CI rule.** This is what
DIP-0011 did. Three years of contributors and one deadline is all it takes
for a Supabase import to land in a core crate with a good reason attached.

## Migration & rollout

Phase 0 (this DIP, docs, positioning) → Phase 1 (signed envelopes and
native identity, DIP-0025) → Phase 2 (P2P market, `--gateway none` gate) →
Phase 4 (Infernet control plane demoted to indexer/gateway). The Infernet
control plane keeps working throughout; it loses authority in Phase 4 and
becomes one of several ways to reach the network, not the way.

Reverting is per-phase. Nothing here deletes a hosted service.

## Open questions

- Which crates are "core" as the tree grows — is `c0mpute-store` core, or
  a service? Its content addressing looks protocol-shaped; its repair
  scheduling does not.
- What the CI dependency check runs on: a `cargo deny` ban list is the
  obvious tool, but the interesting violations are HTTP calls to a
  hardcoded host, which it will not catch.

## Out of scope

- Whether to build the commercial layer, and what it charges. That is
  §19-20 of the v2 direction and a separate decision.
- Token issuance and on-chain consensus. Not proposed, not required.
- The Infernet migration mechanics; Phase 4 gets its own DIP.
