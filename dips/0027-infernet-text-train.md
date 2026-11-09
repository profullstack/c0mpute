# DIP 0027: `infernet.text.train` (LoRA fine-tunes with live progress)

Status: training shipped in c0mpute 0.2.50 (Infernet 0.1.79); chat in 0.2.51 (Infernet 0.1.80).

## What

A buyer fine-tunes a language model on their own corpus on someone else's
machine and watches it train:

```
c0mpute infernet text train --job job.json --dataset corpus.jsonl --yes
looking for a worker for business-strategist-agent ...
training HuggingFaceTB/SmolLM2-135M-Instruct on cpu (60 docs, 5 steps)
eval @0: {"perplexity":3.54}
[########################] step 5/5  loss 1.87  99.1 tok/s
eval @5: {"perplexity":3.50}
adapter sha256:5b1a34b8...  ./adapter  (worker 12D3KooW..., $1.0)
```

A worker opts in with `c0mpute infernet text serve` once its operator has run
`infernet model runtime setup --peft --yes`.

## Job

`job.json` is the `@infernetprotocol/training` job config (`kind: "lora"`,
`base_model` as a Hugging Face repo id, `system_prompt`, `hyperparams`). The
corpus is JSONL of `{title, text}`. The worker rewrites `dataset.url` and
`output.destination_url` to its own job directory; nothing else in the job is
interpreted by c0mpute.

## Protocol

Same offer → bid → accept → receipt flow as every workload.

- Offer `spec_inline` (`TextTrainSpec`): `job`, `dataset_url`, `dataset_hash`
  (`sha256:`), `artifact_upload_url`, `events_url`. Required capability
  `infernet:text:train` (plus `infernet:text:device:cuda` / `c0mpute:ram:Ngb+`
  when asked).
- The buyer serves the corpus and receives uploads and events from a
  token-gated transfer server (the temporal one, with an events route and a
  per-workload artifact list).
- The worker downloads the corpus and refuses it if its hash differs, runs
  `infernet model train --job job.json --json`, and POSTs every
  training-protocol v1 event (`meta`, `step`, `eval`, `checkpoint`, `done`,
  `error`) to `events_url` as it happens. The buyer renders them live.
- On `done: complete` the worker checks the adapter's sha256 against the
  `checkpoint` event, uploads `adapter_model.safetensors`,
  `adapter_config.json` and `manifest.json`, and publishes a receipt whose
  `output_hash` is the adapter hash. The buyer keeps the adapter only if they
  match.
- One job per worker at a time; a busy worker does not bid.

## Safety

- `base_model` must be a repo id: no paths, `..` or URLs.
- The runner reads only local files; the worker downloads (512 MiB cap) and
  hashes the corpus first.
- The transfer URL token gates access; use `--public-url https://...` off-LAN.
- Training runs under the node's job resource limits.

## Chat: `infernet.text.infer` (0.2.51, Infernet 0.1.80)

```
c0mpute infernet text chat --adapter ./adapter "How should I price my SaaS?"
```

- The buyer searches pit.world (`/api/search`) for the question and puts the
  top 3 results in the prompt as numbered background; `--no-search` skips it.
- Offer `spec_inline` (`TextInferSpec`): `adapter_hash`, `base_model` (from the
  adapter's manifest), `messages` (system prompt from the manifest + the user
  turn), `max_new_tokens`, `adapter_url`, `answer_upload_url`.
- The adapter travels as a bundle on the buyer's transfer server and is fetched
  only on a cache miss; the worker keeps it under its cache by hash and refuses
  one whose weights hash differs. Any `text serve` worker on Infernet >= 0.1.80
  advertises `infernet:text:infer`.
- The worker runs `infernet model generate --input request.json --json`, uploads
  `answer.json`, and the receipt's `output_hash` is its sha256, which the buyer
  checks before printing the answer and its sources.

## Replicated verification (opt in)

`--replicas N` (2 to 5; job file `"replicas": N` for `infernet text train`)
runs the same spec on N distinct workers, borrowed from CrowdGPT's pools of
three. The buyer pins `hyperparams.seed` (0 unless set), so the shuffle,
held-out split and LoRA init match; each replica gets its own `run_id` and
only the bid phases are serialized, so each replica's winner is excluded from
the next offer and the training itself runs in parallel.

The buyer downloads every adapter and compares the weight deltas
`dW = B @ A` per module: `d(i,j) = ||dW_i - dW_j||_F / max(||dW_i||, ||dW_j||)`,
computed exactly from `tr((B_i^T B_j)(A_j A_i^T))` without building `dW`.
Two replicas agree when `d <= --replica-tolerance` (default 0.30), their
manifests report the same step count, and their final eval perplexities are
within 10%. A strict majority of the requested replicas must agree pairwise;
the medoid of that group is kept (LoRA factors are not averaged: the average
of the factors is not the average of the deltas). Outliers' workers, and
winners that never delivered, are struck in `<data dir>/train-penalties.json`
and excluded from the buyer's later train auctions. No majority, including a
two-way split, fails the job with the pairwise distances and strikes nobody.

Determinism: replicas are never bit-identical. CPU and CUDA draw different
dropout masks from one seed, and BLAS libraries and thread counts reduce in
different orders, so this is a tolerance check. An untrained adapter is
distance 1.0 from a trained one and an unrelated one about 1.4, well above the
default. Pricing: each replica is a separate job at up to `--max-price`;
default 1 replica keeps today's cost.

## Not yet

- Multi-worker / sharded training.
- Payment settlement beyond the bid price recorded on the receipt.
