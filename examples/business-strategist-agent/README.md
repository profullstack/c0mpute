# business-strategist-agent

Train a small, CPU-only business agent from every feed on an rssamplifier topic page,
e.g. <https://rssamplifier.com/topics/business> (1,663 blogs + 579 news feeds).

| step | file | what it does |
| --- | --- | --- |
| 1 | `harvest.mjs` | topic URL -> feeds -> posts -> full text, written to `data/corpus.jsonl` |
| 2 | `train.py` + `job.json` | LoRA-tunes `SmolLM2-135M-Instruct` on CPU, streams Infernet training-protocol v1 events |
| 3 | `chat.py` | the agent: BM25 retrieval over the corpus + the tuned adapter, answers with cited URLs |

## Run it

```bash
cd examples/business-strategist-agent
uv venv -p 3.12 .venv
uv pip install -p .venv --torch-backend cpu -r requirements.txt   # CPU torch, not the 5 GB CUDA wheel

# 1. corpus: 200 feeds x 10 posts, full article text where the publisher allows it
node harvest.mjs https://rssamplifier.com/topics/business --feeds 200 --posts 10 --full --out data/corpus.jsonl

# 2. train (stdout = NDJSON events, stderr = logs)
.venv/bin/python train.py job.json | tee data/events.ndjson

# 3. ask it something
.venv/bin/python chat.py "How should a 3-person SaaS price its first plan?"
```

Any topic works: swap the URL (`/topics/marketing`, `/topics/startups`, ...) and the
`name` in `job.json`. `--groups blogs,news` is the default; add `podcasts` for show notes.

## What you get

- Model: `SmolLM2-135M-Instruct` + a rank-8 LoRA on q/k/v/o (921,600 trainable params,
  a 3.6 MB adapter). Runs in about 1 GB RAM on 4 threads; no GPU anywhere.
- `out/business-strategist-agent/manifest.json` records the base model, the adapter's
  `sha256` (the same `sha256:<hash>` form c0mpute receipts carry), the dataset and the system prompt.
- Each post is trained as one chat turn ("Brief me on: <title>" -> post text), so the
  instruct model keeps its chat format instead of drifting into raw blog prose.

Measured 2026-11-02 on a shared 32-core box (load ~45) with a 30-feed sample
(111 posts, 32 of them full text, 160 blocks of 256 tokens, 2 epochs):

| | |
| --- | --- |
| wall clock | 6 min 50 s, 4 threads, about 330 tokens/s |
| peak RAM | 1.6 GB (gradient checkpointing; 3.2 GB without it) |
| held-out perplexity | 51.5 -> 45.9 |
| adapter | 3.7 MB safetensors |

The full 200-feed corpus is roughly 15x the tokens: plan on 1 to 2 hours on the same box.

**Set expectations.** 135M parameters learn the register and vocabulary of business
writing, not the facts in thousands of posts. That is why `chat.py` retrieves the
relevant posts and puts them in the prompt: the facts come from retrieval, and the tune
supplies the voice. For real strategy quality, keep the same pipeline and swap
`base_model` for `SmolLM2-360M-Instruct` or `Qwen2.5-0.5B-Instruct` (still CPU-trainable,
just slower).

## Threads

`hyperparams.threads` defaults to 4. Torch's default (one thread per core) on a shared
box is far slower, not faster: at load 58 on 32 cores we measured 22 tokens/s with 32
threads vs about 310 tokens/s with 4.

## On c0mpute

`job.json` is the job config shape from `@infernetprotocol/training`
(`kind`, `base_model`, `dataset`, `hyperparams`, `output`). `train.py` emits that
package's v1 events (`meta`, `step`, `eval`, `checkpoint`, `done`), so it can stand in as
the runner for a text-training backend.

What is **not** there yet: c0mpute ships only the `infernet.temporal.train` and
`infernet.temporal.infer` workloads (DIP 0026, liquid networks), and the training
package's text backends (`deepspeed`, `petals`, `opendiloco`, `openrlhf`) still return
`not_implemented`. To run this job on the network it needs:

1. a `local-cpu` (or `peft`) backend in `@infernetprotocol/training` that spawns `train.py`
   and relays its stdout, so that `infernet model train --job job.json` works;
2. an `infernet.text.train` workload in `plugins/infernet/module.toml` with a
   `c0mpute infernet text train --yes` command, a copy of the temporal one in DIP 0026
   (auction, receipt carrying the adapter hash).

Until then, run it locally as above.

## Data

Everything comes from rssamplifier's public API (no key): `/api/topics/<slug>?group=`,
`/api/feeds/<slug>` and the `read_post` tool on `/api/mcp`, three requests at a time.
`read_post` caches each article, so a publisher is fetched once rather than once per
reader. Posts are the publishers' copyright: keep corpora and adapters trained on them
private unless you have the rights.
