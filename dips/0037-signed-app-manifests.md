# DIP 0037: Signed app manifests (Experts first)

- Status: Accepted (Experts store, first-party only)
- Created: 2026-11-22

## Summary

A single signed manifest format, `c0mpute.manifest/v1`, describes something a store lists and a
node or gateway runs: its publisher identity, the model and prompt or code it runs, the tools it
may call, its pricing, category and version. The first kind is `expert` (a model + system prompt +
allowed MCP plugins applied to a chat turn), shipped as the c0mpute Experts store. Future kinds
(apps, agents, workflows) reuse the envelope.

## Motivation

The board approved a first-party Experts store on 2026-11-22 with one condition from the
Contrarian: build it on the format the open p2p app store will need, so opening it to third
parties later is a policy switch, not a migration.

## Design

- **Envelope fields:** `format`, `kind`, `id` (`<publisher>/<slug>`), `slug`, `name`, `version`
  (semver), `category`, `summary`, `description`, `publisher { id, name, did?, key }`, `pricing`,
  `created_at`, `signature { alg: "ed25519", key, sig }`. Kind-specific fields sit beside them
  (for `expert`: `model`, `system_prompt`, `skills`, `tools.mcp`, `example_questions`,
  `disclaimer`).
- **Signing:** ed25519 over canonical JSON (sorted keys, no whitespace) of the manifest minus
  `signature`. `signature.key` must equal `publisher.key`. Keys are written `ed25519:<base64url>`,
  the raw 32-byte public key, the same encoding the DIP-0036 labeler keys use.
- **Admission is policy:** a store admits a manifest when it is valid, the signature verifies,
  the publisher id and key are on that store's allowlist, and its moderation hook (DIP-0036
  labeler) does not refuse it. c0mpute.com's allowlist is first-party (Profullstack) today.
- **Never listed:** categories `therapy`, `mental-health`, `social-work`, `medical`, `health`
  are refused by the validator (legal and safety hold). `legal` and `tax` require a disclaimer.
- **Pricing:** the manifest carries the publisher's list price; the store's config may
  override it and owns store-wide policy (trial length, bundle price).

## Out of scope (later DIPs)

Gossip/discovery of manifests between nodes, publisher key rotation and revocation records,
revenue share to third-party publishers, and `kind`s other than `expert`.

## Reference

`apps/web/src/lib/experts/manifest.ts` (format, sign, verify, admit), `catalog.ts` (allowlist),
`docs/experts.md` (the store), `scripts/sign-expert-manifests.ts`.
