---
dip: 0030
title: "workflow.step workload and the `c0mpute jobs serve` bridge"
status: Review
authors:
  - riotcoder@profullstack.com
created: 2026-11-02
updated: 2026-11-02
discussion: <PR link>
implementation: <PR link, set when status=Final>
---

# DIP 0030: `workflow.step` and the jobs bridge

## What

meshhook (a webhook workflow engine, n8n-style) is being rebuilt on c0mpute:
each step of a workflow runs on a c0mpute worker instead of on meshhook's own
servers. This DIP adds:

1. A workload, `workflow.step`, on gossip topic `c0mpute/jobs/workflow.step`,
   with two step kinds: `transform` (JMESPath) and `http_call` (one guarded
   outbound HTTP request).
2. `c0mpute jobs serve`, an authenticated HTTP bridge (`POST /v1/jobs`,
   `GET /healthz`) that runs an auction for a job and returns the worker's
   result, verified against the receipt's hash. It is a generic jobs bridge;
   v1 accepts only `workflow.step`.

The wire contract between c0mpute and meshhook is "contract v1" below; both
sides are written against it.

## Spec (offer `spec_inline`)

```json
{ "v": 1, "kind": "transform" | "http_call", "config": { ... }, "input": <any JSON> }
```

- `transform`: `config.expression` is a JMESPath string, applied to `input`
  (Rust `jmespath` crate; meshhook uses the JS `jmespath` 0.16 library, both
  standard JMESPath).
- `http_call`: the requester has already resolved its templates. `config =
  { url, method, headers?: {k: v}, body?: string | JSON, timeout_ms? }`.
  A JSON (non-string) body is serialized and sent as `application/json`
  unless the spec sets `Content-Type`. Output: `{ "status": <int>, "headers":
  {lowercase k: v}, "body": <parsed JSON when the content type is JSON and it
  parses, else text> }`. Any HTTP status is `ok: true`; transport failures
  are `ok: false`.

A spec is at most 1 MiB. `v != 1`, an unknown `kind` or a malformed config
completes with an `ok: false` result rather than a failed receipt, so the
requester sees why.

Specs over 32 KiB do not fit a gossip message (gossipsub caps messages at
64 KiB and the spec rides in both the offer and the accept). The bridge then
stores the spec in its chunk store, announces it, and sends
`{"spec_ref": {"hash": <blake3 hex>, "bytes": n}}` inline instead; the winning
worker fetches the chunk from the buyer (the offer's publisher) and refuses it
unless it hashes to `hash` and is at most 1 MiB. This is internal to c0mpute;
the bridge's HTTP API is unchanged.

## Result

Stored in the worker's chunk store, named by the receipt's `output_hash`,
fetched by the buyer from the winner and checked against that hash, exactly
like speech results:

```json
{ "v": 1, "ok": true,  "output": <any JSON>,  "duration_ms": 12 }
{ "v": 1, "ok": false, "error": "message",   "duration_ms": 12 }
```

A result is at most 4 MiB. A receipt marked `Failed` means the worker itself
broke (it could not fetch a large spec or store the result).

## Bridge

```
C0MPUTE_JOBS_TOKEN=<24-256 chars> c0mpute jobs serve [--listen 127.0.0.1:8788] [--max-price 0] [--bid-window-ms 2000]
```

- `C0MPUTE_JOBS_ALLOW_PEERS=peerA,peerB`: optional global bidder allowlist.
  A request's `allow_peers` may narrow it but not widen it (400).
- `GET /healthz` → `{ "ok": true, "peer_id": "...", "workloads": ["workflow.step"] }`.
- `POST /v1/jobs` (Bearer) `{ "workload", "spec", "max_price"?, "allow_peers"?, "timeout_ms"? }`
  - 200 `{ "job_id", "worker", "price", "output_hash", "result" }`
  - 400 `{ "error": "bad_request" | "unknown_workload", "detail" }`
  - 401 `{ "error": "unauthorized" }`
  - 503 `{ "error": "no_bids", "detail" }`: no worker bid inside the window
    (or no peer is on the topic at all)
  - 502 `{ "error": "worker_failed", "detail", "job_id", "worker" }`: failed
    receipt, missing hash, hash mismatch, or a result that is not the schema
  - 504 `{ "error": "timeout" }`: no receipt, or the result could not be
    fetched, before `timeout_ms`
- `max_price` defaults to the bridge's `--max-price` and can only lower it.
  `timeout_ms` defaults to 60 000 and is clamped to 1 000..900 000; it covers
  the bid window, the run and the result fetch.
- The bridge does not retry. An `http_call` may not be idempotent; retrying
  is the requester's decision.

Hardening matches `c0mpute speech serve`: required bearer token compared in
constant time, loopback bind by default, bounded request bodies (spec +
64 KiB), at most 32 jobs in flight.

## Workers

New `Role::WorkflowStep` (`workflow-step` in config and on `--roles`),
advertising `c0mpute:role:workflow-step`; buyers require that tag. It is on by
default (in `Config::default()` and added by `worker start` without
`--roles`, like TTS/STT) because both kinds are cheap and need no install.
Steps run on their own 16 slots, not the node's CPU slots, so they neither
wait on nor delay transcodes. Pricing is the operator's normal listing
(`c0mpute pricing`), like every workload.

The status aggregator lists `workflow.step` in `KNOWN_WORKLOADS`, so seed
nodes subscribe to and relay the topic for buyers that only know the seeds.

## Security model

- **No code from the network runs.** There is no script, template or plugin
  step kind. `transform` is a JMESPath search; `http_call` is one request.
- **JMESPath bounds.** Expression at most 16 KiB, bracket nesting at most 64,
  at most 512 operators outside literals (the parser and interpreter recurse
  per operator; unbounded chains overflowed the stack in testing). The search
  runs on its own thread with a 64 MiB stack and a 10 s budget, and output is
  serialized through a capped writer (4 MiB), so fan-out expressions such as
  `@ | [@, @] | [@, @] ...` fail fast instead of allocating without bound.
- **SSRF guard for `http_call`.** http/https only. The host is resolved and
  every address must be public: loopback, RFC 1918, link-local (which covers
  169.254.169.254), CGNAT, multicast, broadcast, unspecified, documentation,
  benchmarking and reserved space, IPv6 ULA/link-local/site-local/multicast,
  and IPv4-mapped/compatible IPv6 are refused; NAT64 and 6to4 are judged by
  their embedded IPv4. `localhost` names are refused before resolution. The
  connection is pinned to the vetted addresses (`resolve_to_addrs`), so a
  second DNS answer cannot rebind it, and proxies from the environment are
  ignored. Redirects are followed manually, at most 5, and every hop is
  resolved and vetted again; credentials (`Authorization`, `Cookie`) are
  dropped when a redirect changes origin. The spec may not set framing or
  routing headers (`Host`, `Content-Length`, `Transfer-Encoding`,
  `Connection`, ...). Response bodies are capped at 1 MiB (no decompression),
  the timeout defaults to 15 s and is capped at 30 s.
  The same guard (shared `net_guard` module) now also covers transcode input
  URLs.
- **Payloads are visible to the winning worker.** The spec travels in gossip
  (or by hash to the winner) in clear: inputs, URLs, headers and bodies,
  including any API keys a step carries. Requesters must keep secrets off the
  network, or pin trusted workers with `allow_peers` /
  `C0MPUTE_JOBS_ALLOW_PEERS`. Results are verified against the receipt hash,
  which proves the bytes came from the winner, not that the winner computed
  them honestly.
- **Operator exposure.** A worker with the role makes outbound requests to
  public addresses on a stranger's behalf, from its own IP. Operators who do
  not want that run `--roles` without `workflow-step`.

## Alternatives considered

- **Raise gossipsub `max_transmit_size` to carry 1 MiB specs inline.** Old
  nodes and seeds would drop the larger messages, and every subscriber would
  receive the full spec for every offer. Carrying large specs by hash needs
  no protocol change and only the winner downloads them.
- **Arbitrary code steps (JS/WASM).** Out of scope for v1; it needs a
  sandbox story and per-step resource metering first.
- **Retries in the bridge.** Rejected: side-effecting HTTP steps are not
  idempotent.
