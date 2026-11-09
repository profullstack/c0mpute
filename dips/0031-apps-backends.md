---
dip: 0031
title: "Apps: containerised backends with leaderless 3x redundancy, and git over the network"
status: Review
authors:
  - riotcoder@profullstack.com
created: 2026-11-09
updated: 2026-11-09
discussion: <PR link>
implementation: <PR link, set when status=Final>
---

# DIP 0031: Apps

## What

Hosting (DIP-0015) keeps static sites alive. Apps keep *running code* alive:
an owner-signed record names a container image and the network keeps
`replicas` (default 3) copies of it, runs exactly one, snapshots its state to
the others, and fails over by itself. Every gateway serves it. `git push
c0mpute://<name>` stores a repository on the network and deploys it.

The goal is to replace a VPS for ordinary backends (APIs, MCP servers, bots,
workers with a SQLite database) without a coordinator, a registry or a cloud
account. User docs: `docs/apps.md`.

## Wire

Topic `c0mpute/apps/v1`, JSON, `t`-tagged:

- `record { record: AppRecord }`: owner-signed (ed25519 over
  `"c0mpute-app-record\n" || json(record with sig="")`). `app_id =
  blake3("c0mpute-app\n{owner}\n{name}")[..20]`. Highest `seq` wins;
  `deleted: true` is a tombstone.
- `node { x25519, free_mb, hosts, apps: [HeldApp], addrs }` every 10 s.
  `HeldApp = { app_id, seq, role: primary|standby, epoch, snapshot?, healthy,
  has_secrets, ready }`.
- `seal { app_id, secrets_hash, to, from_pub, ct }`: `AppSecrets` sealed with
  X25519 → HKDF-SHA256(`apps:{app_id}:{secrets_hash}`) → AES-256-GCM. Opened only
  if the gossip source is the owner's peer or a current holder and the
  plaintext hashes to the record's `secrets_hash`.

RPCs: `apps.http` (an `HttpReq` to the primary, answered with `HttpResp`),
`apps.logs` (owner peer only), `apps.record` and `hosting.record` (a record by
id, for clients that just joined).

## Decisions, all local and deterministic

- Holders: keep the live holders; fill the shortfall with the top rendezvous
  ranks (`blake3("c0mpute-apps-rank\n{app}\n{peer}")`) among live hosting nodes
  with `free_mb >= memory_mb`; above `replicas`, the lowest-ranked standby lets
  go and does not retake the app for 4 TTLs.
- Primary: a live claimant keeps it; among several, the highest
  `(epoch, rank)` wins and the others stop. With none for `node_ttl` (35 s),
  the ready holder with secrets and the newest snapshot `(epoch, n)` promotes
  at `max(epoch) + 1` once it has listened for `warmup`. Nodes never resume
  as primary after a restart.
- State: the primary pauses, runs `docker cp <state> -`, and unpauses; the tar
  is cut into 1 MiB pieces, each AES-256-GCM under
  `derive_key("c0mpute apps snapshot v1", app_key)` with nonce
  `keyed_blake3(piece)[..12]` (convergent: unchanged pieces are the same
  chunk). Standbys fetch the manifest and missing pieces over chunk-fetch.
  RPO = `snapshot_secs`.

## Not in this DIP

Multi-writer or distributed SQL (rqlite/dqlite as an app needs node-to-node
links between holders), websockets and streaming bodies, payment for
running apps, confidential execution.
