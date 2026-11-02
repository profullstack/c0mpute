# business-strategist-agent

Train a small, CPU-only strategy agent from links: rssamplifier topic pages (every feed
filed under them), pit.world searches, single feeds or any web page. Run the training on
your own CPU or on the c0mpute network, then publish the adapter to Hugging Face.

| step | file | what it does |
| --- | --- | --- |
| 1 | `harvest.mjs` | links -> documents in a JSONL corpus |
| 2 | `train.py` + `job.json` | LoRA-tunes `Qwen2.5-0.5B-Instruct` on CPU, streams Infernet training-protocol v1 events |
| 2 (network) | `c0mpute infernet text train` | the same job on a c0mpute worker, with a live progress bar |
| 3 | `chat.py` | the agent: pit.world search for the question (corpus BM25 offline) + the adapter, answers with cited URLs |
| 4 | `publish.py` | pushes the adapter + a model card to the Hugging Face Hub (private by default) |

## Run it: saas-business-strategist-agent

```bash
cd examples/business-strategist-agent
uv venv -p 3.12 .venv
uv pip install -p .venv --torch-backend cpu -r requirements.txt   # CPU torch, not the 5 GB CUDA wheel

# 1. corpus from rssamplifier topics AND pit.world searches (comma lists expand to one per term)
node harvest.mjs https://rssamplifier.com/topics/business,saas,crypto \
  'https://pit.world/search?q=saas,business,crypto' --full --out data/saas-business.jsonl

# 2a. train on the c0mpute network (job-saas.json carries the name and system prompt)
c0mpute infernet text train --job job-saas.json --dataset data/saas-business.jsonl \
  --output out/saas-business-strategist-agent --yes
#     the worker must reach your machine: add --bind 0.0.0.0:<port> --public-url http://<reachable ip>:<port>

# 2b. or train on this CPU
.venv/bin/python train.py job.json --name saas-business-strategist-agent --dataset data/saas-business.jsonl

# 3. ask it something
.venv/bin/python chat.py --model out/saas-business-strategist-agent "How should a 3-person SaaS price its first plan?"

# 4. publish (free; private unless --public)
HF_TOKEN=hf_... .venv/bin/python publish.py out/saas-business-strategist-agent --repo <user-or-org>/saas-business-strategist-agent
```

### What `harvest.mjs` accepts

| link | becomes |
| --- | --- |
| `rssamplifier.com/topics/<a>[,<b>,...]` | every feed filed under each topic (`--feeds 200` per topic, `--groups blogs,news`) -> their recent posts (`--posts 10`), full text with `--full` |
| `rssamplifier.com/<feed-slug>` | that feed's recent posts |
| `pit.world/search?q=<a>[,<b>,...]` | each term searched on the web, news and scholar tabs (`--pit-tabs`, or the link's `&t=`); every result page fetched, its snippet if the page blocks us |
| any other URL | the page itself |

Pass links as arguments, in a file (`--links links.txt`) or pasted on stdin
(`pbpaste | node harvest.mjs --links - --full`). Duplicates (same URL or title) are dropped.

## What you get

Measured 2026-11-02 on a shared 32-core box (load ~45, ~4 GB free RAM):

| | business sample | saas-business-strategist-agent |
| --- | --- | --- |
| base model | SmolLM2-135M-Instruct | Qwen2.5-0.5B-Instruct (bf16) |
| corpus | 111 posts (30 feeds) | 3,615 posts (business 1,514, crypto 1,471, saas 630) |
| steps | 160 (2 epochs) | 400 (batch 2 x 256 tokens) |
| held-out perplexity | 51.5 -> 45.9 | 44.5 -> 28.6 |
| peak RAM | 1.6 GB | 2.4 GB |
| adapter | 3.7 MB | 4.4 MB |

`out/<name>/manifest.json` records the base model, the adapter's `sha256` (the form c0mpute
receipts carry), the dataset, its sources, the hyperparameters and the system prompt.
Each document is trained as one chat turn ("Brief me on: <title>" -> text), so the instruct
model keeps its chat format.

**Set expectations.** A sub-1B model learns the register and vocabulary, not the facts in
thousands of posts. `chat.py` therefore searches pit.world for the question and puts the
top pages in the prompt: the facts come from search, the tune supplies the voice. The
135M model echoed questions back; 0.5B answers on topic ("One monthly price, one annual
price at 10 times the monthly, and no other discounts", citing three SaaS-pricing guides),
but still leans on the snippets it is given. A 1.5B base is the next step up and still CPU-trainable.

## Run it in Ollama

The published model is on Hugging Face as a GGUF, so Ollama (0.3.13 or newer; update with
`curl -fsSL https://ollama.com/install.sh | sh` on Linux, `brew upgrade ollama` on a Mac) runs it directly:

```bash
ollama run hf.co/profullstack/saas-business-strategist-agent-GGUF "How should a 3-person SaaS price its first plan?"
```

To make a GGUF of your own adapter (Ollama takes safetensors LoRA adapters only for Llama,
Mistral and Gemma bases, so a Qwen adapter is merged into the base first):

```bash
git clone --depth 1 https://github.com/ggml-org/llama.cpp ../llama.cpp
uv pip install -p .venv -e ../llama.cpp/gguf-py sentencepiece
.venv/bin/python export_gguf.py out/saas-business-strategist-agent --llama-cpp ../llama.cpp
ollama create saas-strategist -f out/saas-business-strategist-agent-gguf/Modelfile
```

`export_gguf.py` writes a Q8_0 GGUF (531 MB for the 0.5B model) and a Modelfile with the
agent's system prompt. Uploaded to Hugging Face with `system` and `params` files beside it,
`ollama run hf.co/...` picks those up too. On its own, without search results in the prompt,
the 0.5B model answers in the right register but invents details; `chat.py` is the better front end.

## Surviving a shared box

`train.py` saves `out/<name>/resume.pt` every 25 steps and resumes from it when rerun with
the same arguments, so an OOM killer costs minutes, not the run. earlyoom killed the
saas run twice (exit 143, no traceback); a retry loop finished it:

```bash
until .venv/bin/python train.py job.json --name saas-business-strategist-agent --dataset data/saas-business.jsonl; do sleep 20; done
```

## Threads

`hyperparams.threads` defaults to 4. Torch's default (one thread per core) on a shared
box is far slower, not faster: at load 58 on 32 cores we measured 22 tokens/s with 32
threads vs about 310 tokens/s with 4.

## On c0mpute

Text fine-tunes are a c0mpute workload since 0.2.50 (`infernet.text.train`, DIP 0027, with
Infernet 0.1.79's `peft` backend). `job.json` is the `@infernetprotocol/training` job shape
(`kind`, `base_model`, `dataset`, `hyperparams`, `output`, plus `name` and `system_prompt`),
and `train.py` emits the same v1 events (`meta`, `step`, `eval`, `checkpoint`, `done`) the
network runner does, so a job runs the same way on your CPU or on a worker.

```bash
# worker (once): infernet model runtime setup --peft --yes && c0mpute infernet text serve
# buyer:
c0mpute infernet text train --job job-saas.json --dataset data/saas-business.jsonl --output out/saas-business-strategist-agent --yes
```

The buyer runs a small token-gated transfer server that the worker posts progress events
and the adapter to, so the worker must be able to reach it. On a box whose firewall drops
inbound ports, either open one port (`--bind 0.0.0.0:47556 --public-url
http://<public ip>:47556`) or join the worker's Tailscale network and pass the tailnet IP.
The receipt's output hash is the adapter's sha256, checked on arrival.

## Data

Everything comes from public APIs (no key): pit.world `/api/search`, and rssamplifier's `/api/topics/<slug>?group=`,
`/api/feeds/<slug>` and the `read_post` tool on `/api/mcp`, three requests at a time.
`read_post` caches each article, so a publisher is fetched once rather than once per
reader. Posts are the publishers' copyright: keep corpora and adapters trained on them
private unless you have the rights.
