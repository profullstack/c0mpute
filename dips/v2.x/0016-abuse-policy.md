---
dip: 0016
title: "Abuse policy for hosted content: CSAM hash blocklist, per-worker opt-in"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-05-20
updated: 2026-05-20
discussion:
  - https://github.com/profullstack/c0mpute/discussions
implementation:
supersedes:
superseded-by:
---

## Summary

Before c0mpute markets the hosting vertical (DIP-0015) as "takedown
resistant" to the public, the network needs a stated, narrow,
machine-enforceable abuse policy. The proposal:

1. **A single category is network-blocked at the protocol level: CSAM.**
   Nothing else — not copyright, not defamation, not political
   speech. The line stays narrow and defensible.
2. **Enforcement is per-worker opt-in/out, not network-mandated.**
   Workers choose to honor the network-maintained blocklist. The
   default for the reference client is "opt in"; operators can flip
   it off and accept the legal risk themselves.
3. **The blocklist is content hashes only.** A public, auditable log
   of blocked `blake3:...` digests. Anyone can verify what's
   blocked; nobody can use it as a covert takedown channel.
4. **Tier interaction is honest:** the `verified` tier is checked
   at PUT time; the `private` tier (customer-encrypted) cannot be
   checked by design, and that liability sits with the customer.

This DIP targets **v2** of the hosting vertical — the moment we
publicly market takedown resistance. v1 (DIP-0015 Phases 1–3) can
ship as hash-addressed beta with no public takedown-resistance
positioning, before this is finalized.

## Motivation

Every uncensorable storage network that didn't decide this in advance
got pinned to the wall later. Storj and Filecoin both improvised
under pressure and shipped policies that satisfied neither the
"uncensorable" community nor the "no CSAM on our infra" expectation
that workers, lawyers, payment processors, and journalists all hold.

Three forcing functions:

1. **Worker survival.** A worker operator running a home rig who
   ends up hosting a CSAM shard is in real legal jeopardy in most
   jurisdictions, regardless of whether they knew what the shard
   was. Without a credible protocol-level filter, sober operators
   won't run workers, and we lose supply.
2. **Payment processor risk.** CoinPay's banking partners will not
   tolerate a network with zero abuse posture. "We're decentralized"
   is not an answer that survives a single phone call from a card
   network.
3. **Press / regulatory risk.** "c0mpute is the new dark web file
   host" is the lazy story a reporter writes if there's no
   articulable policy. Articulable policy + narrow scope + public
   audit log = different story.

The narrow-scope answer is the one that holds: block what is
universally illegal across all liberal-democracy jurisdictions
(CSAM), refuse to block anything else, publish the list of what's
blocked. That position is defensible against both "you're
censoring" critique and "you're hosting illegal content" critique.

## Detailed design

### Scope: CSAM only

The network-maintained blocklist contains hashes of known CSAM
material **only**. Not:

- Copyrighted material (DMCA / equivalents are a customer-level
  concern, not a network-level one)
- Defamation or politically disfavored speech (the whole point of
  the vertical)
- Terrorism propaganda (real category, but enforcement requires
  judgment that hash-matching can't make; out of scope)
- Sanctions-listed content (jurisdictional and impractical to
  centralize)

This narrow scope is the *load-bearing* property. Once "and also X"
gets added, the network becomes the arbiter of X, and the
takedown-resistance promise erodes in proportion. We commit, in
this DIP and in public marketing, to keeping the scope at one
category.

### Source of hashes

Two acceptable sources for the v2 blocklist:

1. **NCMEC PhotoDNA / PDQ hashes** — accessed via a vetted operator
   relationship. Standard industry route used by Cloudflare, Google,
   Microsoft. The hashes are perceptual (PDQ) or cryptographic
   (PhotoDNA); we'd publish only the SHA-256 / blake3 of the *file*
   when seen, not the perceptual hashes themselves.
2. **IWF (Internet Watch Foundation) URL+hash list** — same shape,
   UK-anchored, similar vetting requirements.

In practice we ship with whichever we can get a license for first.
The blocklist format is provider-agnostic:

```json
{
  "kind": "blocklist_entry",
  "object_hash":   "blake3:...",
  "category":      "csam",
  "source":        "ncmec",
  "added_ts":      "2026-05-20T13:00:00Z",
  "source_ref":    "ncmec:case-id-redacted",
  "sig":           "c0mpute-blocklist-signer-sig"
}
```

The blocklist is itself a signed, append-only log, published as a
c0mpute-hosted object with a well-known name. Workers fetch it on
a schedule (hourly).

### Per-worker enforcement

Workers honor or ignore the blocklist via a config flag:

```toml
# c0mpute.toml
[host]
enforce_blocklist = true   # default
blocklist_sources = ["c0mpute.com/blocklist/v1"]
```

When `enforce_blocklist = true`, the worker:

- On `PUT /storage/v1/objects/<hash>` (verified tier): rejects with
  `451 Unavailable For Legal Reasons` if `<hash>` is on the
  blocklist.
- On scheduled scan: if any shard whose parent object is on the
  blocklist is currently held, deletes the shard, emits a
  `blocklist_eviction` attestation, and forfeits accrued
  reservation payouts for that object.

When `enforce_blocklist = false`, the worker accepts and serves
everything. The reference CLI prints a clear warning at startup if
this is set, and an operator who flips it does so with full
knowledge that they're carrying their own legal exposure.

**Crucially:** the network does not slash or de-peer
non-enforcing workers. They participate in everything else
normally. The blocklist is a property of the *worker's chosen
policy*, not of network membership.

### Tier interaction (the honest part)

| Tier | What the worker sees | Can it be checked? |
|---|---|---|
| `cheap` (3-copy) | plaintext bytes + plaintext object hash | yes |
| `verified` (RS 10/14, server-side at-rest enc) | plaintext bytes pre-shard, hash matches plaintext | yes |
| `private` (RS 10/14, customer-encrypted) | ciphertext only; object hash is hash of ciphertext | **no, by design** |

For `private` tier, the network has no way to detect CSAM — that
is the actual point of customer-side E2E. Liability for the
plaintext sits with whoever holds the key, not the workers.
Documentation and ToS must make this explicit. v2 marketing should
not pretend otherwise.

A reasonable v2 default may be: **`private` tier is gated behind
identity verification on the customer DID** (CoinPay-side
KYC-lite). Not because we want to know who customers are, but
because the abuse exposure on `private` content is severe and a
verified DID provides a downstream legal anchor if the network is
later subpoenaed. Open for debate — see open questions.

### Public audit log

The blocklist is fully public. Specifically:

- `https://c0mpute.com/blocklist/v1` returns the signed JSON log.
- Each entry contains `object_hash`, `category`, `source`,
  `source_ref`, `added_ts`, signer signature.
- The blocklist itself is an immutable append-only log (a c0mpute
  hosted object whose root is anchored in a CoinPay signed-pointer
  record, per DIP-0015 name layer).
- Historical entries cannot be removed silently. A delisting
  appears as a new entry (`kind: "blocklist_delisting"`).

This is what prevents the blocklist from being weaponized. If
someone with influence tries to slip a non-CSAM hash onto the list,
it appears in public, immediately, with a signature.

### Appeals

The blocklist will have false positives. The appeals path is
asymmetric — the cost of staying blocked is "your content is
inaccessible via enforcing workers"; the cost of being wrongly
listed is real but bounded.

Procedure (v2):

1. The customer (or anyone) emails a designated address with the
   `object_hash` and an attestation that the content is not CSAM.
2. A small, named review pool examines the hash against the
   source's record. Anonymous when needed, but the *pool* is
   public (named individuals or named organizations).
3. If the listing is wrong, a `blocklist_delisting` entry is
   appended. The original entry stays in the log with the delisting
   noted.

We do not commit to a turnaround SLA before having one in
practice. Honest "best effort" framing in v2.

### Legal reporting

A worker that detects a blocklist-hit upload is in a complicated
position under US law (18 U.S.C. § 2258A — reporting obligation
for electronic communication service providers) and various EU
equivalents. The v2 default behavior:

- The worker logs the event locally (object_hash, peer-id of the
  uploader, timestamp).
- The worker does **not** automatically forward to NCMEC or
  equivalent. We do not want to be in the position of routing
  reports for operators who haven't consented.
- The reference client surfaces a "report this incident"
  affordance to the operator with prefilled NCMEC submission
  fields. Operators choose to file or not.

This may need to change depending on legal advice — flagged as an
open question.

### What stays out of the protocol

- **Per-worker custom blocklists** are fine and out of scope here.
  An operator who wants to also refuse copyrighted content, or
  refuse anything from a particular DID, configures that locally.
  The protocol does not encode it.
- **Network-level political content moderation** — explicitly never.
  This DIP commits to scope creep being the failure mode, not the
  outcome.
- **Per-jurisdiction enforcement** — a worker may choose to refuse
  content that's illegal where they sit (e.g., a German worker
  refusing certain historical content). That's a worker-local
  policy, not a network-level one.

## Alternatives considered

**No policy; pure neutrality.** "We're a protocol, not a publisher."
This is the Tor / IPFS bet. It works for tiny purist communities
and dies on contact with worker supply, payment processors, and
press cycles. Not viable for a network that wants to bill paying
customers in dollars through banks.

**Centralized takedown panel.** A c0mpute-operated body that
adjudicates removal requests across categories. Defeats the
vertical's value prop and turns the company into a censorship
authority. Hard pass.

**Match into the storage primitive directly** (every PUT scanned,
network-mandated). Same as above with an automated front-end —
expands scope inevitably (today CSAM, tomorrow copyright, next
year political content depending on jurisdiction). Per-worker
opt-in keeps the policy boundary intact.

**Cryptographic filtering on `private` tier.** Private Set
Intersection / homomorphic match against the blocklist without
seeing plaintext. Promising research territory but not deployable
at this scale in 2026. Revisit when the cryptography matures.

**Trust the source list completely; no audit log.** Less work, but
removes the protection against the blocklist being weaponized.
The audit log is what makes the narrow-scope commitment credible.

## Migration & rollout

This DIP targets **v2** of the hosting vertical. Concretely:

- **v1 (DIP-0015 Phases 1–3, beta).** Hash-only addressing. No
  `c0mpute://` name marketing. No public "censorship-resistant"
  positioning. Closed-beta customer list. Abuse exposure is bounded
  because the surface area isn't public yet.
- **v2 (DIP-0015 Phase 4+ goes public).** Name registry, browser
  extension story, public marketing of takedown resistance. This
  DIP must be Accepted and implemented before that switch flips.

Implementation phases for this DIP:

1. **License a source list** (NCMEC or IWF). Six-to-twelve-month
   path, real legal work.
2. **Ship `enforce_blocklist` config + scheduled fetch + PUT-time
   match.** Reference client gets the flag, default `true`.
3. **Publish the blocklist endpoint + audit log format.** Anchor
   the latest blocklist root via the DIP-0015 name layer once that
   ships.
4. **Stand up the appeals pool.** Named reviewers + email intake
   + delisting workflow.
5. **Public marketing of v2 launches only after 1–4 are in
   place.**

## Open questions

- **License path.** NCMEC vs IWF vs both. Different vetting bars.
  Needs legal counsel input before commitment.
- **`private` tier + identity verification.** Should `private`
  tier require a verified DID to mint a reservation? Trades
  customer privacy against abuse exposure. The fact that we
  can't filter `private` content makes some downstream anchor
  desirable. Strong opinions both ways.
- **Reporting obligation passthrough.** Whether the reference
  client should automate NCMEC reporting on detection vs leave it
  to operators. US lawyers may push for automation; civil-liberties
  community will push hard against. Likely punt = surface the
  affordance, don't automate the submission.
- **False-positive rate from perceptual hashes.** If we use PDQ /
  PhotoDNA-derived lists, the rate is non-zero. We must publish
  what we measure once we have operating data.
- **Worker reputation interaction.** Currently the DIP says
  non-enforcing workers are not slashed. Open question: are they
  marked in some publicly visible way so customers can choose to
  route around them? Probably yes, but the surface needs design.
- **Multi-source merge.** If we license two lists with conflicting
  records, what's the merge policy? Most likely: union, with each
  entry retaining its source.

## Out of scope

- Any category beyond CSAM at the network-policy level.
- Custom worker-level blocklists (allowed, not specified here).
- Customer-side DMCA processing — handled by the customer with
  the content owner, never by the network.
- Per-country geofencing — a worker may refuse content locally;
  the protocol does not coordinate this.
- Tooling for detecting CSAM uploads that *aren't* on the
  blocklist (i.e., novel material). Out of scope at the protocol
  level; remains an operator-level concern using off-the-shelf
  scanners if they choose.
- Anything related to `private` tier plaintext — by design, the
  network cannot see it.
