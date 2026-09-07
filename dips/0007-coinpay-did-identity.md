---
dip: 0007
title: "CoinPay DID is the canonical identity, payment, and reputation layer"
status: Accepted
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-05-03
discussion: docs/c0mpute-v1.md
implementation:
supersedes: 0003
superseded-by:
---

> **Amended by DIP-0025 (Draft).** The *identity* half of this DIP no longer
> holds: network identity is now a natively generated `did:c0mpute:z…`, so a
> peer can be discovered and verified with no payment product in the path. The
> *payment, escrow, settlement and reputation* half stands — CoinPay is the
> default settlement adapter and the first-party integration. What changed is
> that it is no longer load-bearing for the network to run. See
> `docs/protocol/identity.md`.

## Summary

Every actor on the c0mpute network — buyer, worker, validator, organization
— has a CoinPay DID:

```
did:coinpay:buyer:abc123
did:coinpay:worker:def456
did:coinpay:validator:ghi789
did:coinpay:org:jkl012
```

The DID binds together wallets, payment history, reputation, stake,
optional KYC/KYB credentials, hardware attestations, signed receipts,
and dispute history. CoinPay is the only system that holds long-lived
key material on a node; `c0mpute` and `infernet` ask CoinPay for
signatures via subprocess (or a small SDK that talks to a local
`coinpay daemon`).

This supersedes DIP-0003 (raw nostr pubkey identity).

## Motivation

Three things forced this:

1. **Payments need a real layer.** Nostr signed-requests cover request
   auth but not escrow, payouts, slashing, or dispute resolution. Adding
   those on top of bare nostr means rebuilding most of CoinPay anyway.
2. **Reputation is identity-shaped.** Worker reputation is anchored to a
   stable identity that survives across jobs, machines, and wallet
   rotations. CoinPay DID gives that; raw nostr pubkeys conflate
   identity with key material.
3. **Other Profullstack marketplaces want the same primitives.** CoinPay
   is meant to be reused beyond c0mpute. Building it once and reusing
   beats building three half-systems.

## Detailed design

### What a DID binds

```
did:coinpay:<role>:<id>
   ├── controller pubkey(s)        # rotation-friendly
   ├── linked wallet addresses
   ├── reputation score(s) per role
   ├── completed-jobs counters
   ├── failure / dispute counters
   ├── stake amount + lockup terms
   ├── optional KYC / KYB credential reference
   ├── optional hardware attestation credential reference
   └── slashing history
```

Roles in the DID URI distinguish what the actor is doing — the same
human can have a `:worker:` DID and a `:buyer:` DID; they're different
identities for marketplace purposes even if they share underlying
wallets.

### What a DID can prove

- **Same actor over time** — reputation persists across machines.
- **Control of the controller key** — sign-and-verify on any payload.
- **Job lineage** — every job is signed-by-DID; every receipt is
  signed-by-DID; the chain is auditable.

### What a DID cannot prove (and what we pair it with)

| DID can't prove… | Paired mechanism |
|---|---|
| Output correctness | Validator sample + ffprobe / VMAF / schema checks |
| Worker honesty | Sandbox + signed runner image digests |
| GPU authenticity | Hardware attestation credential (later) |
| Private data protection | Encrypted inputs + restricted egress (private tier) |
| Model-actually-used | Model hash in manifest + spot-check duplicate exec |
| Runtime integrity | Runtime image hash + signed receipt chain |

### Wire format: signed-request envelope

We carry forward DIP-0003's wire format (which mirrors infernet-protocol's
`@infernetprotocol/auth`), but the `pubkey` field becomes a DID
reference:

```
X-Coinpay-Auth: base64url({
  v: 1,
  did: "did:coinpay:worker:abc123",
  key_idx: 0,                    # which controller key signed (rotation)
  created_at: 1714737600,
  nonce: "<random>",
  sig: "<schnorr-or-ed25519>"
})
```

Verifier resolves the DID to its current controller key set, picks
`key_idx`, and verifies the signature.

The canonical signing string is unchanged from DIP-0003:

```
<METHOD>\n<path>\n<created_at>\n<nonce>\nsha256(body)
```

### Local daemon

CoinPay ships as a binary that can run as a one-shot CLI (`coinpay did
status`, `coinpay escrow create ...`) and optionally as a long-lived
local daemon listening on a Unix domain socket:

```
~/.config/coinpay/daemon.sock
```

When `c0mpute worker start` runs, it talks to that daemon for every
sign / verify / escrow operation rather than spawning `coinpay` per
operation. Daemon mode is opt-in via `coinpay daemon start`.

### Identity bootstrap

```bash
coinpay did create
# generates ~/.config/coinpay/identity.key (chmod 600)
# prints did:coinpay:user:abc123
# prompts: also create a worker / buyer / validator DID? (y/n)
```

`c0mpute worker register` requires a `:worker:` DID; if none exists,
prompts the user to create one through `coinpay did create --role worker`.

### Reputation badges (per PRD §"Reputation Requirements")

Surface badges, not opaque scores:

```
431 completed jobs
98.7% validation success
$500 staked
KYC verified (optional)
H100 attested (optional)
No slashing events in 90 days
```

These render in the dashboard's worker-detail page and on the trust
inspect command:

```bash
c0mpute trust inspect did:coinpay:worker:def456
coinpay reputation inspect did:coinpay:worker:def456
```

### Trust tiers (per PRD)

The five tiers (cheap / standard / verified / private / enterprise) are
expressed as policy on the buyer side: a job manifest can declare
`required_tier: "verified"` and the scheduler filters workers
accordingly. Tiers are computed from DID-bound metrics + optional
credentials.

## Alternatives considered

**Plain nostr (DIP-0003).** Identity but no payments. We'd end up
building CoinPay anyway.

**OAuth/OIDC.** Not portable across marketplaces; doesn't model worker
reputation or stake natively; weak fit for headless nodes.

**Ethereum-style account-as-DID (`did:ethr:...`).** Tempting but ties
the system to one chain. CoinPay is escrow-agnostic from day one.

**No DID — opaque server-issued account IDs.** Simplest to implement,
but reputation and stake become non-portable, and a CoinPay outage means
no one can sign jobs.

## Migration & rollout

Greenfield in c0mpute. CoinPay is being built alongside; this DIP
locks the integration shape.

Order:

1. `coinpay did create` writes a local key + registers with the CoinPay
   API to allocate a `did:coinpay:user:...`.
2. `c0mpute worker register` requests a `:worker:` DID derived from the
   user DID.
3. Job manifests reference DIDs in `buyer`, `worker`, `validator`
   fields.
4. Receipts are signed-and-counter-signed across DIDs.
5. Reputation tables key on DID strings.

## Open questions

- **DID method spec.** Do we publish the `coinpay` DID method as a real
  W3C-compatible spec, or treat it as an internal convention? Probably
  publish, but doesn't block v1.
- **Key rotation UX.** How does a user replace a compromised controller
  key without losing reputation? CoinPay needs a rotation endpoint and
  a recovery story (email-of-record? mnemonic?). Worth its own DIP.
- **Custodial vs self-custody escrow.** v1 likely runs custodial escrow
  through CoinPay's own ledger; long-term we want on-chain escrow as an
  option. Out of scope here.

## Out of scope

- Specific cryptographic curve choices (Schnorr vs ed25519) — covered in
  CoinPay's own design doc.
- The CoinPay billing/payout schedule — that's a CoinPay concern.
- Cross-marketplace DID interoperability — assumed to fall out of CoinPay
  being shared infrastructure.
