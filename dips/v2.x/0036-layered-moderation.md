---
dip: 0036
title: "Layer moderation: network, operator-chosen labelers, space authority"
status: Draft
authors:
  - anthony@profullstack.com
created: 2026-11-22
updated: 2026-11-22
discussion:
implementation:
supersedes:
superseded-by:
---

## Summary

DIP-0016 settles what the *network* blocks: CSAM hashes only, enforced
per worker, published in a public log. It deliberately says nothing about
everything else: phishing kits on free hosting, malware droppers, spam on
a site's board, an off-topic post in a group. Today the answer is
"nothing, or whatever an operator hand-rolls". This DIP fills that gap
without widening DIP-0016's scope, borrowing the layered model AT
Protocol published for its spaces ("Moderation in spaces", atproto.com,
2026-11-20):

1. **L0 Network** - the DIP-0016 CSAM log. Unchanged.
2. **L1 Operator** - each worker and each gateway chooses which
   *labelers* it subscribes to and what it does with each label
   (refuse, warn, ignore). One operator's choice binds only that
   operator's node.
3. **L2 Space authority** - the owner of a name (and so of a site and
   every app under it: `bbs.<name>`, `social.<name>`, ...) moderates
   their own space with roles, space-scoped labels and a reports inbox.
   Space labels stay inside the space; they are never gossiped and no
   worker acts on them.

Removal only flows downward: losing a higher layer removes you from the
lower ones, never the reverse.

## Motivation

- **Free hosting is an abuse magnet.** `hosting_push` publishes a site
  through the shared cloud node at no cost, and `gateway.c0mpute.com`
  serves it on a c0mpute hostname. The first sustained abuse will be
  phishing pages and malware links on that domain, and that domain is
  ours. Its reputation, and CoinPay's banking relationships, carry the
  cost. DIP-0016 can't help, by design: phishing is not CSAM.
- **Workers need a lever short of "carry everything".** DIP-0016's
  argument for worker survival applies to more than CSAM. A home
  operator who doesn't want to serve credential-harvesting kits should
  be able to say so with a config line, not a fork.
- **Name owners need tools.** Moshpit names already get a board
  (`bbs.<name>`) and an AT Proto feed (`social.<name>`). Owners have no
  shared way to hand out moderator roles, take reports, or hide a post
  without taking down the account. Every app reinvents it, or nobody
  moderates.
- **Doing it centrally is the failure DIP-0016 warns about.** A
  c0mpute-run takedown panel for "everything else" would make the
  network the arbiter of X. Pushing decisions to the operator (L1) and
  the space owner (L2) keeps the network neutral.

## Detailed design

### The removal rule

| Removed at | Effect |
|---|---|
| L0 (blocklisted hash) | refused by every enforcing worker; gone wherever enforcement is on |
| Name revoked at the registry | the name stops resolving everywhere; every space under it goes dark |
| L1 (one worker refuses) | that worker drops its replica; the site lives on other replicas (DIP-0015 reconstitution) |
| L1 (one gateway refuses) | that gateway won't serve it; peer gateways and direct fetch still work |
| L2 (owner removes a member) | the member's DID loses access to *that space only*; their identity, names and other spaces are untouched |
| L2 (owner hides or removes content) | gone from the space; the account that posted it stays |

The reverse never holds. A space banning a DID never touches the
registry. A worker refusing a site never revokes its name.

### Labels (L1)

A label is a signed statement by a labeler about a subject. The shape
follows `com.atproto.label` closely enough to bridge later (see
Alternatives):

```json
{
  "kind": "label",
  "ver":  1,
  "src":  "did:...labeler",
  "uri":  "blake3:<object> | blake3:<site_root> | name:<name> | url:<https url>",
  "val":  "phishing",
  "neg":  false,
  "cts":  "2026-11-22T13:00:00Z",
  "exp":  "2026-12-22T13:00:00Z",
  "evidence": "optional opaque ref, e.g. threatcrush:incident-id",
  "sig":  "<labeler signature over the canonical record>"
}
```

- `neg: true` retracts an earlier label with the same `src`/`uri`/`val`.
- `exp` is required for every value except the ones a labeler marks
  `permanent` in its manifest. Reputation goes stale, so labels expire.
- Each labeler publishes a **manifest**: its DID, label values with a
  one-line definition each, its appeal address, and its log URL. A
  labeler's log is append-only and public, the same audit property as
  DIP-0016. Nobody can quietly label a site.

Labelers are not special to the protocol. Anyone with a DID can run one.
c0mpute ships no built-in labeler other than the DIP-0016 log, which is
an L0 source and keeps its own rules.

### Operator policy (L1)

```toml
# c0mpute.toml
[moderation]
labelers = []            # reference-client default: none beyond DIP-0016
# labelers = ["threatcrush.com/labels/v1"]

[moderation.actions]     # per label value; unknown values = ignore
malware  = "refuse"
phishing = "refuse"
spam     = "ignore"
```

Actions:

- **`refuse`**
  - *worker:* reject the `PUT` with `451` and drop held shards on the
    next scan. Emit a `policy_refusal` attestation naming the label, so
    the refusal is not counted as a challenge failure. Accrued payout
    for that object is forfeited, as in DIP-0016. The replica
    reconstitutes on a non-refusing worker.
  - *gateway:* answer `451` with a page that names the labeler, the
    label value and the labeler's appeal address.
- **`warn`** (gateway only): serve an interstitial with the same
  information and a continue link.
- **`ignore`**: do nothing.

No layer above the operator can force an operator to subscribe to a
labeler. A worker that subscribes to nothing is not slashed and not
de-peered, the same commitment DIP-0016 makes for its blocklist.

### The canonical gateway and the shared cloud node

`gateway.c0mpute.com` and the shared cloud node behind free
`hosting_push` are operated by us, so they get an L1 policy like any
other operator:

- Subscribe to the ThreatCrush labeler (below) for `malware`,
  `phishing` and `cryptominer`, all `refuse`.
- Run the check at `hosting_push` time (the push is refused, with the
  reason) and at serve time (a label can arrive after the push).
- Publish the gateway's own action log: every subject refused, which
  label, when, and when it was lifted.

This is one gateway's policy, not network policy. Peer gateways, direct
fetch and paid replicas on other workers are unaffected. That honesty is
what keeps DIP-0016's narrow promise intact.

### ThreatCrush as the first labeler

ThreatCrush already runs CrushAV hash reputation and IP/domain
reputation. It publishes a labeler manifest and log with:

- `malware` - an object hash hits CrushAV.
- `phishing` - a site root or URL matches phishing reputation or page
  heuristics.
- `cryptominer` - in-browser mining script detected.

Appeals go to ThreatCrush's address in the manifest, and a reversal is a
`neg` label in its log. Building the labeler endpoint is ThreatCrush
work and gets a ThreatCrush issue. This DIP only fixes the record format
it must emit.

### Space authority (L2)

A **space** is a name plus everything served under it: the site, and any
app keyed to the name (`bbs.<name>`, `social.<name>`, future ones). The
**space authority** is the owner DID on the name's latest
`name_pointer` record (DIP-0015 name layer, Moshpit names alike).

The authority signs a **roles record**, stored in the space:

```json
{
  "kind": "space_roles",
  "space": "name:<name>",
  "roles": {
    "did:...alice": ["label", "read_reports", "resolve_reports"],
    "did:...bob":   ["label"]
  },
  "cts": "2026-11-22T13:00:00Z",
  "sig": "<owner signature>"
}
```

The three default roles are the ones atproto defines for groups:

- **`label`** - apply a space label to a post, file or member.
- **`read_reports`** - see the reports inbox.
- **`resolve_reports`** - act on a report: hide or remove content,
  remove a member, dismiss.

The owner holds all roles implicitly and can always revoke.

**Space labels** use the label shape above with
`src = <role-holder DID>` and are stored in the space's own store, not
in any public log. Apps in that space must honor them. Workers and
gateways ignore them by design: publishing even *references* to a
space's moderation would leak the space. This is the one place where
labels are deliberately not public, and it is safe because space labels
have no effect outside the space.

**Reports** go to `POST /.well-known/c0mpute/report` on any gateway
route for the name, or to an app's own report button. The body is
encrypted to the space authority's key, and the gateway forwards it
without reading or storing it. It lands in the space's reports queue,
where `read_reports` holders see it.

**Removal inside a space** happens in the apps: tsbb hides a post or
bans a DID from `bbs.<name>`, pit-social drops a DID from the
`social.<name>` feed, and a static site removes a path by publishing a
new site root. None of it reaches L1 or L0.

### Failure modes covered

- *A labeler goes rogue or gets compromised.* Its log is public,
  operators unsubscribe, and labels expire.
- *A labeler goes offline.* Operators keep the last good copy of the
  log until `exp`, then drop the labels. Content fails open, not closed.
- *Someone abuses the space report endpoint.* Reports are rate-limited
  per source at the gateway. The gateway never decrypts them, so a
  flood costs the space owner triage time, not privacy.
- *The owner loses their key.* Recovery follows the owner DID's own
  recovery path (DIP-0007). Moderation can't outlive ownership.

## Alternatives considered

**Add categories to the DIP-0016 blocklist.** This is the simplest
route, and it is exactly the scope creep DIP-0016 names as its failure
mode. It would make c0mpute the arbiter of phishing, then spam, then
whatever is next. Rejected.

**A c0mpute-run takedown desk.** DIP-0016 already rejected it for the
same reasons: a single point of pressure, and it turns the company into
a censor.

**Adopt atproto space and label lexicons directly.** This is
attractive, since pit-social already speaks AT Proto. But the group
standard behind spaces (opensocial.group) is still a proposal, and the
atproto post defines no lexicons for space roles or reports. Deferred.
The label record above keeps atproto's field names (`src`, `uri`,
`val`, `neg`, `cts`, `exp`, `sig`) so a bridge stays cheap once that
work settles.

**Gateway-only enforcement (no worker policy).** Simpler, but it
leaves home operators with no lever. Workers carry the storage risk, so
they get the choice.

## Migration & rollout

1. **Label record + manifest + verification library**
   (`c0mpute-host::labels`). Nothing enforces yet.
2. **ThreatCrush labeler** goes live with `malware`, `phishing` and
   `cryptominer` (a ThreatCrush issue).
3. **Canonical gateway and shared cloud node** honor it, at push and at
   serve time, with the action log published. This is the step that
   matters most and should come first after 1-2.
4. **Worker `[moderation]` config** in the reference client. The
   default stays empty, so nothing changes for anyone who doesn't opt
   in.
5. **Space authority:** roles record, report endpoint and queue, first
   in tsbb (`bbs.<name>`) and pit-social (`social.<name>`).

Revert path: steps 3 and 4 are config. Emptying `labelers` returns a
node to today's behavior. No data migration.

## Open questions

- **Reference-client default.** Empty (pure opt-in, as written) or
  ThreatCrush pre-filled for `malware` only? Opt-in is the honest
  default. Pre-filled protects naive operators.
- **Marking refusing workers.** DIP-0016 asks the same question. Should
  a site owner see that some of their replicas refused under policy, and
  why? Probably yes, in `hosting_push_status`.
- **Label vocabulary governance.** Each labeler defines its own values.
  Do we want a small shared core (`malware`, `phishing`, `spam`) so
  operator configs carry across labelers?
- **Where the reports queue lives** for a static site with no app
  backend: in the owner's c0mpute DM inbox, or in a hosted object only
  the owner can decrypt?
- **atproto interop** once opensocial.group publishes lexicons: map the
  roles record and reports onto them, or bridge?

## Out of scope

- Anything DIP-0016 covers (CSAM), which stays at L0 with its own rules.
- Name registry policy: when a Moshpit or c0mpute name can be revoked
  belongs to the registry, not here.
- `private` tier plaintext, which nobody but the key holder can see,
  same as DIP-0016.
- Copyright and DMCA, a matter between the customer and the rights
  holder. A labeler *may* define such a value, and an operator *may*
  subscribe, but the protocol takes no position.
- Automated reporting to authorities, which stays operator-initiated as
  in DIP-0016.
