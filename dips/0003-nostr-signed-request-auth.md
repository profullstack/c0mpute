---
dip: 0003
title: "Nostr-keyed signed-request envelope for operator + provider auth"
status: Superseded
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-05-03
discussion:
implementation:
supersedes:
superseded-by: 0007
---

> **Superseded by DIP-0007** (CoinPay DID as the identity layer). The c0mpute
> v1 PRD makes CoinPay DID the canonical identity, trust, payments, escrow,
> and reputation system across the network. Nostr-style signed-request
> envelopes are still a viable wire format inside CoinPay (the canonical
> signing string and replay-protection mechanics carry forward), but the
> identity scheme is `did:coinpay:...`, not raw nostr pubkeys, and key
> material flows through CoinPay rather than being managed independently
> by each component.

## Summary

Adopt the signed-request envelope pattern proven in
`@infernetprotocol/auth` for Quest:

- **Provider nodes** authenticate every coordinator API call with a
  Schnorr (BIP-340) signature over a canonical request string. No bearer
  tokens, no API keys to leak.
- **Operators** can log into the dashboard either via Supabase magic-link
  *or* via NIP-07 browser-extension signing.
- **Identity = pubkey.** A node's libp2p peer-id is derived from the same
  ed25519 keypair we already generate at first run; operator dashboard
  identity is a separate secp256k1 keypair (the nostr public key).

We do **not** ship a relay client or publish nostr events in v1. This DIP
covers identity and request authentication only — the public attestation /
gossip use cases are deferred.

## Motivation

We need an auth story that:

1. Doesn't put a long-lived bearer secret on every provider node (those
   nodes are run on consumer hardware, sometimes by people we'll never
   onboard via support).
2. Lets operators recover identity without us — a pubkey is portable,
   a database row in our Supabase isn't.
3. Plays well with the existing crypto skill set in the depin.quest
   ecosystem (infernet is already on this pattern).
4. Doesn't lock customers' viewers into yet another login.

Bearer tokens fail #1 and #2. OAuth fails #2 and #3. NIP-07 + signed-
requests covers all four.

## Detailed design

### Provider node ↔ coordinator

Every API call from `depin video` to the coordinator carries:

```
X-Depin-Auth: base64url({ v, pubkey, created_at, nonce, sig })
```

Where the canonical signing string is:

```
<METHOD>\n<path>\n<created_at>\n<nonce>\nsha256(body)
```

Identical to infernet's wire format — letting us re-use their primitives
package nearly verbatim. The coordinator validates:

1. `created_at` within ±60s of server clock
2. `nonce` not in replay cache (LRU + Redis-backed once we cluster)
3. Schnorr signature verifies under `pubkey`
4. `pubkey` is registered (or auto-register-on-first-use for new providers
   per the `providers` table)

### Operator dashboard login

Two paths, both produce the same Supabase JWT downstream:

- **NIP-07** (browser extension — Alby, nos2x): dashboard calls a
  `/video/api/v1/auth/nostr/challenge` endpoint to get a server-issued
  random string, signs it via `window.nostr.signEvent(...)`, sends back
  for verification, gets a Supabase session cookie.
- **Magic link** (existing Supabase flow): unchanged for users who don't
  have a nostr extension.

A profile row may have *both* a Supabase user_id and a nostr pubkey; either
can drive a session.

### Identity tables (additive to 0001_init.sql)

```sql
alter table profiles
  add column nostr_pubkey text unique;

create index profiles_nostr_pubkey_idx on profiles(nostr_pubkey)
  where nostr_pubkey is not null;
```

### Provider-side keypair

The Rust node already plans to generate `~/.depin/identity.key` (PRD §16).
This DIP proposes:

- That file holds an ed25519 keypair used both for libp2p peer-id and for
  signing coordinator requests. We use ed25519 (not secp256k1) here because
  libp2p peer-ids derive cleanly from it; the dashboard's nostr login
  remains secp256k1/Schnorr because that's what NIP-07 extensions speak.
- Coordinator accepts both signature schemes on the request envelope,
  distinguished by a curve byte in the auth header.

Rationale for the asymmetry: we want operators to use *any* nostr extension
for dashboard login, but we control the node binary and don't want every
node to ship a Schnorr implementation (ed25519 is in the Rust stdlib of
crypto crates we already use for libp2p).

### Replay protection

In-memory LRU per coordinator instance (mirrors infernet) — fine for v1
single-instance deploys. Multi-instance: move to Supabase or Redis. Note
this in the `Open questions` until we cluster.

## Alternatives considered

**Pure bearer tokens.** Simpler implementation, fails the
"no-long-lived-secret-on-every-provider-machine" requirement.

**OAuth/OIDC with a real IdP.** Solves operator login cleanly but adds a
heavyweight dep we don't need; provider nodes still need *something*
non-OIDC for headless API calls.

**ed25519 across the board including dashboard login.** Cleaner single
algorithm story, but rules out NIP-07 extensions, which is the whole point
of leaning on the nostr ecosystem. Not worth it.

**Skip nostr entirely; just JWT-from-Supabase.** Works for dashboard, fails
for headless nodes. We'd end up reinventing signed-request anyway.

## Migration & rollout

Greenfield — no users to migrate.

Rollout order:
1. Add the `@infernetprotocol/auth`-equivalent package to the coordinator
   (or vendor their primitives — license check first).
2. Add the auth header verification middleware in `apps/coordinator`.
3. Generate identity in `c0mpute-core::supervisor::boot` before the first
   coordinator call.
4. Ship NIP-07 login as an additive button on the dashboard sign-in page,
   parallel to magic-link.

Backwards compatibility: requests without the `X-Depin-Auth` header keep
hitting the existing `x-quest-user-id`/`x-quest-provider-id` shim used in
the coordinator scaffolds today. We remove that shim once all callers have
migrated.

## Open questions

- License of `@infernetprotocol/auth` — do we vendor, depend, or
  reimplement? (TODO check their package.json before merging this DIP.)
- Do we want `NIP-42`-style relay auth for any future relay-publish
  use case? Probably yes when that lands, but out of scope here.
- Provider account recovery: if a node loses `~/.depin/identity.key`,
  what's the path? Generate a new one, re-register, lose stake history?
  Or do we let an operator bind a new pubkey to an existing provider row
  via a dashboard flow signed by their nostr operator key?

## Out of scope

- Publishing public attestations of jobs to nostr relays (deferred).
- Nostr-based payouts / zaps (CoinPayments owns money — see PRD §15).
- Encrypted DMs or any nostr communication between viewers and providers.
- A native mobile signer flow (browser-extension-only for v1; see infernet
  pitfall #2).
