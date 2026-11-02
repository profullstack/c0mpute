# DIP 0027: `infernet.text.train` (LoRA fine-tunes with live progress)

Status: shipped in c0mpute 0.2.50 with Infernet 0.1.79.

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

## Not yet

- Serving a trained adapter for chat (`infernet.text.infer`).
- Multi-worker / sharded training.
- Payment settlement beyond the bid price recorded on the receipt.
