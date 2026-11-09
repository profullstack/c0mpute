---
dip: 0032
title: "End-to-end encrypted jobs: only the winning worker sees a job, only the buyer sees its result"
status: Accepted
authors:
  - riotcoder@profullstack.com
created: 2026-11-09
updated: 2026-11-09
discussion: <PR link>
implementation: <PR link, set when status=Final>
---

# DIP 0032: End-to-end encrypted jobs

## What

Every job on the network (chat and inference, TTS, STT, image, music and
video generation, embeddings, OCR, transcode, render, crawl, `workflow.step`,
text training and inference, temporal, webhook, gameserver, captcha) is now
end-to-end encrypted between the buyer and the one worker that wins its
auction:

- the offer that goes to gossip carries only what bidding needs;
- the job itself is sealed to the winner and handed over point to point;
- inputs too big for the job (audio, images, documents, corpora, adapters,
  `.blend` files) are stored as ciphertext under a per-input key that only
  the winner learns;
- the result is sealed back to the buyer before the worker stores it.

## Why

Job topics (`c0mpute/jobs/<workload>`) are gossipsub topics. Every peer and
relay subscribed to a topic received every offer on it, and until this DIP an
offer carried the whole job inline: every prompt, every transcript, every
URL, to every node on the topic, not just the one that won. Inputs and
results were plaintext chunks that anyone holding the hash (which the offer
and the receipt published) could fetch. Node operators had access to
people's data. That kept sensitive work (CoinPay's finance AI, for one) off
the network.

## The protocol

```text
buyer  -> gossip  offer    e2e: { v: 1, buyer_key: B, public_spec }
                           spec_inline: absent
                           spec_hash: salted commitment to the spec
                           required_capabilities: [..., "c0mpute:e2e:v1"]
worker -> buyer   bid      e2e: { worker_key: W, sig: ed25519(identity, binding) }
                           (direct and on gossip, as before)
buyer  -> both    accept   names the winner, e2e: 1, no spec
winner -> buyer   rpc      c0mpute.job.payload.v1 (request-response)
                           the buyer answers only the authenticated winner
                           with the spec sealed to W
winner -> store   result   sealed to the buyer; the receipt names the
                           ciphertext's hash
```

### Keys

- `B` is a fresh X25519 key per auction; `W` a fresh X25519 key per bid.
  Neither is ever reused, so a later compromise of either side opens nothing
  old.
- The worker signs the binding of `W` to the job with its libp2p ed25519
  identity: `lp(domain) lp("bid") lp(job_id) lp(workload) lp(buyer_peer)
  lp(worker_peer) lp(B) lp(W)` (every field length-prefixed). The buyer
  verifies it against the bidder's peer id, which inlines the ed25519 key,
  before it seals anything to `W`. A bid without a valid signature is
  ignored.
- `shared = X25519(B, W)`, refused if it is a low-order point.
  `HKDF-SHA256(salt = "c0mpute/job-e2e/v1", ikm = shared)` expands, with the
  same length-prefixed binding as `info`, to a spec key and a result key.

### The sealed spec

`nonce(12) || AES-256-GCM(spec_key, salt(32) || spec)` with the binding as
associated data, so a sealed spec cannot be replayed into another job,
workload, buyer or worker. The offer's `spec_hash` is
`blake3_derive_key("c0mpute/job-e2e/v1 spec commitment", salt || spec)`; the
winner checks the opened spec against it, and checks that every field of the
offer's `public_spec` equals the same field recomputed from the opened spec,
so a buyer cannot bid out a small job and hand over a big one. It then runs
the workload's bid filter again on the whole spec (all the checks a filter
used to make on a plaintext offer still run before a job starts). Any
failure ends the job with a `Failed` receipt so the buyer re-auctions at
once.

The buyer holds the sealed spec only until its auction ends, and serves it
only to the peer the accept named; the caller's peer id comes from the noise
handshake. Request-response responses carry up to 10 MiB, so the old
`spec_ref` (large specs stored as a plaintext chunk the winner fetched by
hash) is no longer used by any buyer.

### Sealed blobs (inputs and results)

`magic "C0JOBE1\0" || salt(32) || segments`. The blob key is
`HKDF(salt, root)`; segment `i` (64 KiB of plaintext) is AES-256-GCM with
nonce `last(1) || 0(3) || i(8)` and the header as associated data. The last
segment carries the flag, so truncation, reordering and splicing all fail.
Files are sealed and opened streamed.

- Results: `root` = the result key. `publish_result_bytes` /
  `publish_result_file` seal before storing (render frames and text adapters
  too). Buyers open through `gateway_util::fetch_result` /
  `fetch_result_to_file` and `AuctionOutcome::open_result`.
- Inputs: `root` = a random key per input
  (`put_bytes_pieces_sealed` / `put_file_pieces_sealed`), carried as
  `<prefix>_key` (`audio_key`, `image_key`, `input_key`, `corpus_key`,
  `adapter_key`) inside the sealed spec. Workers open with
  `InputRef::fetch_opened` / `fetch_to_file_opened`.

Receipts are unchanged: `output_hash` names the ciphertext, which is all a
validator or billing system needs.

### Public specs

A per-workload allowlist of small scalar fields, plus size buckets (powers of
two): model, engine, voice, locale, language, width, height, steps, frames,
format, lang, frame range, render engine and device, `max_tokens`,
`base_model`, webhook retry budget, gameserver size, and so on. Never
prompts, texts, messages, URLs, headers, input hashes or keys, file names,
client keys or tokens (`gameserver.stop` shows `sha256(token)`, which is
already the server's public stop hash). Strings over 128 bytes never go in a
public spec whatever the key. An unknown workload shows only `v`.

## What the winning worker can still see

The winner must see plaintext to compute on it. It decrypts the spec in
memory, inputs in memory or in the job's scratch directory (removed when the
job ends, on failure paths too), and never logs content. It keeps the
plaintext result only until it is sealed. A sealed `.blend` stays sealed in
the render cache; a sealed adapter is kept only for its job, never in the
adapter cache.

A dishonest winner can still copy what it computes on. Two stronger options:

1. **Private mode** (`c0mpute private`, already shipped): bid only for, and
   accept only, your own or allowlisted nodes. Use it for sensitive work.
2. **Confidential computing** (`with_confidential(true)` on an auction,
   capability `c0mpute:confidential`): only workers running in an attested
   TEE (AMD SEV-SNP, Intel TDX) can win. The offer carries a fresh
   `attest_nonce`; the bid carries attestation evidence whose report data is
   `blake3_xof(job_id, worker_peer, W, nonce)`, binding the TEE to the very
   key the spec is sealed to; the buyer verifies it (`AttestationVerifier`)
   before sealing. Workers produce evidence through `AttestationProvider`
   and only advertise the tag when one is configured. **Shipped:** the
   requirement, tags, report-data binding and both hooks, failing closed (a
   confidential auction with no verifier gets no winner). **Deferred:** the
   concrete SNP/TDX evidence producer and verifier (VCEK / PCS collateral,
   measurement policy); no node in the fleet runs in a TEE today.

## What observers can still see

Metadata: that a job of a given workload ran, its public spec, its price,
timing, which peers bid and which won, and the sizes (as buckets for the
public spec, exactly for ciphertext chunks). `infernet.text.train` progress
events (step, loss, throughput) still go to gossip under the job's run id.

## Compatibility

- New buyers send only e2e offers, with `c0mpute:e2e:v1` in
  `required_capabilities`. Workers older than this release check that they
  advertise every required tag, so they never bid; buyers ignore bids
  without a signed key anyway.
- New workers still serve plaintext offers from old buyers for one release
  (`C0MPUTE_REJECT_PLAINTEXT_OFFERS=1` refuses them now). The next release
  removes plaintext offers.
- New workers still open inputs without a `<prefix>_key` (plaintext chunks a
  client stored itself, as the jobs bridge allows).

## Alternatives considered

- **Encrypt the offer to every eligible worker.** Leaks the job to every
  bidder, not just the winner, and needs a worker directory with keys.
- **Seal on the accept and gossip it.** Ciphertext on gossip is safe, but a
  multi-MiB spec does not fit a 64 KiB gossip message, and gossip fans it to
  every node for nothing. Point to point is smaller and does not need the
  `spec_ref` detour.
- **Static worker keys.** Simpler, but a stolen worker key would open every
  job it ever won. Per-bid keys signed by the identity give forward secrecy
  for the cost of one signature per bid.
- **Transport-only (noise) plus access control on chunk fetch.** Protects
  the wire but leaves plaintext at rest on workers and buyers, and relies on
  every node enforcing the ACL.
