---
dip: 0028
title: "webhook — paid, end-to-end encrypted webhook delivery"
status: Final
authors:
  - anthony@profullstack.com
created: 2026-11-02
updated: 2026-11-02
discussion:
implementation: node/crates/c0mpute-core/src/webhook.rs, node/crates/c0mpute-cli/src/webhook.rs
supersedes:
superseded-by:
---

# DIP 0028: `webhook.deliver` (paid, end-to-end encrypted webhooks)

Status: shipped in c0mpute 0.2.53.

## What

A buyer sends a webhook from someone else's machine and IP, pays per
delivery, and nobody between them and that one worker can read it:

```
c0mpute webhook send https://example.com/hook \
  -d '{"event":"invoice.paid"}' -H 'x-tenant: acme' \
  --secret whsec_... --attempts 3
looking for a webhook worker (max $0.002) ...
200 in 183ms after 1 attempt(s)
{"ok":true}
(worker 12D3KooW..., $0.001, receipt sha256:2485a5ea...)
```

A worker opts in with `c0mpute webhook serve [--price 0.001] [--concurrency 8]`.
The plugin is in-process (compiled into `c0mpute`); `plugins/webhook/module.toml`
lists it on c0mpute.com/plugins and `/api/plugins`.

## Protocol

Everything rides the normal offer → bid → accept → receipt market on topic
`c0mpute/jobs/webhook.deliver`. Gossip is readable by every subscriber, so the
offer carries nothing sensitive:

```jsonc
// JobOffer.spec_inline
{ "v": 1, "client_pubkey": "<buyer X25519, fresh per job>",
  "max_attempts": 3, "timeout_ms": 10000, "backoff_ms": 1000 }
```

After the accept:

| step | from → to | message | content |
|---|---|---|---|
| 1 | worker → buyer | `webhook_ready` | worker's fresh X25519 key, echo of `client_pubkey` |
| 2 | buyer → worker | `webhook_request` | sealed `{url, method, headers, body, secret?, id?}` |
| 3 | worker → buyer | `webhook_result` | sealed `{delivered, status, attempts, latency_ms, total_ms, response_headers, response_body, truncated, webhook_id, error, log[]}` |
| 4 | worker → buyer | `JobReceipt` | `output_hash = sha256:` of the sealed result string |

Crypto is the stack sealed temporal sessions already use
(`temporal::crypto`): X25519 → HKDF-SHA256 → AES-256-GCM with a random nonce.
There is one key per direction, bound to the job id
(`c0mpute.webhook/v1/<job>/request|result`), so a result cannot be replayed as
a request and nothing moves between jobs. No new crypto dependency; `hmac` is
added only for the Standard Webhooks signature.

Who may talk to whom:

- the buyer seals only to the bidder it accepted (the accept is reported to the
  sender through `JobAuction::with_accept_notify`), and only on a
  `webhook_ready` whose gossip source (gossipsub runs Signed + Strict) is that
  bidder;
- the worker opens only a `webhook_request` whose gossip source is the offer's
  buyer; and it only runs accepts that match the bid it placed (buyer, exact
  spec including `client_pubkey`, `external_ref`, price within the cap: the
  0.2.47 accept checks), so a forged accept cannot swap in another key;
- the buyer keeps the result only if its sha256 equals the receipt's
  `output_hash`.

Relays, the market and every other node see the retry budget, the two public
keys and ciphertext. The e2e test runs a bystander node on the topic and
asserts the URL, headers, body and secret never appear on the wire.

What it does not hide: the winning worker necessarily sees the plaintext (it
makes the request), and the target sees the worker's IP.

## Delivery and SSRF guard

- http(s) only, no credentials in the URL, methods POST/PUT/PATCH/DELETE/GET.
- The host is resolved on every attempt; every address must be public: no
  loopback, RFC 1918, link-local / cloud metadata (169.254/16), CGNAT
  (100.64/10), 0/8, 240/4, multicast, documentation ranges, ULA, IPv4-mapped
  and IPv4-compatible IPv6, NAT64 (64:ff9b::/96). The connection is pinned to
  the checked addresses (`resolve_to_addrs`) so DNS cannot rebind between the
  check and the connect. A refusal is final and never retried.
- Redirects are never followed; a 3xx is the answer and goes back to the buyer
  with its `location` header.
- Per-attempt timeout (1–30 s) covers connect, headers and body. The response
  body is read only up to 16 KiB (`truncated: true` past that).
- The buyer may not set `host`, `content-length`, `transfer-encoding`,
  `connection` and other hop-by-hop headers, nor `webhook-*`; header values
  cannot carry CR/LF.

The same `is_public_ip` now also rejects IPv6 multicast, IPv4-compatible,
NAT64 and documentation ranges for transcode's input fetch.

## Signing

With `--secret`, the worker adds Standard Webhooks headers (no house scheme
existed): `webhook-id` (from `--id`, else `msg_<job id>`, the same on every
retry), `webhook-timestamp` (per attempt) and
`webhook-signature: v1,<base64 HMAC-SHA256(key, "<id>.<timestamp>.<body>")>`,
where `key` is the base64 after `whsec_` (any other secret is used as raw
bytes). Tested against the spec's reference vector.

## Retries

1–5 attempts in total. Retry on no answer (connect error, timeout), 408, 425,
429 and 5xx; never on 2xx, 3xx or other 4xx. The first retry waits
`backoff_ms` (100–10 000), each later one doubles it, capped at 30 s. The
worker refuses to bid when the offer's deadline cannot hold the worst-case
budget (attempts × timeout + backoffs).

## Size limits

A sealed request or result must fit one gossipsub message (64 KiB), so: body
≤ 32 KiB, ≤ 32 headers totalling ≤ 8 KiB, URL ≤ 2 KiB, secret ≤ 256 bytes, and
the sealed request ≤ 60 000 bytes after JSON escaping (checked before the
auction). A result that would not fit has its response body trimmed. Bigger
payloads would need the transfer-server path (DIP-0027) and are out of scope.

## Price

Paid per delivery: one job = one webhook with all its retries.

- Default worker price: **$0.001 per delivery**, fixed (`--price`). Workers bid
  exactly their price, never the buyer's cap.
- Default buyer cap: **$0.002 per delivery** (`--max-price`).
- No free path: a worker cannot serve below **$0.0001** (`serve` refuses), it
  never bids on an offer whose cap is under its price, and the sender refuses a
  cap under $0.0001. `--external-ref coinpay:inv_...` carries a billing id on
  offer, accept and receipt.
- Receipts: `Completed` when the target answered (any status: the work was
  done), `Failed` when nothing could be sent (refused address, unreachable on
  every attempt, no sealed request). Both carry the sealed-result hash so the
  buyer learns why.
- Settlement follows the c0mpute market: the agreed price is on the accept and
  receipt; the 5% market fee applies when market settlement launches (not
  collected today, see /pricing).

## Open items (decisions for Anthony)

1. The $0.001 default price and $0.002 default cap.
2. Whether a `Failed` (refused/unreachable) delivery is billable. Today it is
   not marked Completed.
3. Payloads over 32 KiB (transfer-server path), and `worker start --roles`
   integration (today a webhook worker is its own `c0mpute webhook serve`
   process with its own node identity).
