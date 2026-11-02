#!/usr/bin/env python3
"""Publish the GGUF from export_gguf.py to <repo>-GGUF on the Hugging Face Hub.

    HF_TOKEN=hf_... .venv/bin/python publish_gguf.py out/not-a-lawyer-agent --repo profullstack/not-a-lawyer-agent --public

Uploads the GGUF plus `system` and `params`, which `ollama run hf.co/<repo>-GGUF`
reads, so Ollama users get the agent's system prompt and sampling settings
without a Modelfile. Private unless --public, like publish.py.
"""

import argparse
import json
import os
from pathlib import Path

from huggingface_hub import HfApi

PARAMS = {"temperature": 0.3, "repeat_penalty": 1.25, "num_predict": 350}


def card(m, repo, gguf):
    hp = m.get("hyperparams", {})
    ev = m.get("eval") or {}
    net = m.get("trained_on")
    tags = ", ".join(["gguf", "ollama", "llama.cpp", *(m.get("tags") or []), "cpu", "c0mpute"])
    where = "on the c0mpute network" if net else "on a CPU"
    ppl = f", held-out perplexity {ev['perplexity_before']:.1f} to {ev['perplexity_after']:.1f}" if ev else ""
    receipt = " (matches the c0mpute receipt)" if net else ""
    disclaimer = f"\n> **{m['disclaimer']}**\n" if m.get("disclaimer") else ""
    q = m.get("example_question", "")
    return f"""---
base_model: {m['base_model']}
tags: [{tags}]
license: apache-2.0
pipeline_tag: text-generation
---

# {m['name']} (GGUF)
{disclaimer}
[{repo}](https://huggingface.co/{repo}) merged into {m['base_model'].split('/')[-1]} and quantized ({gguf.stat().st_size / 1e6:.0f} MB). Runs on a CPU.

```bash
ollama run hf.co/{repo}-GGUF {json.dumps(q) if q else ''}
```

Ollama picks up the system prompt (`system`) and sampling settings (`params`) from this repo.
llama.cpp: `llama-cli -hf {repo}-GGUF`.

Trained {where}: {m['documents']} documents from rssamplifier.com topics and pit.world search, {m['steps']} LoRA steps (r={hp.get('lora_r')}){ppl}. Adapter sha256 `{m['adapter_sha256']}`{receipt}.
Every Infernet and c0mpute node pulls this model automatically ([house models](https://infernetprotocol.com/docs/house-models)).
It answers best with fresh search results in the prompt (`w3bs ask`, or the example's `chat.py`); a 0.5B model supplies the voice, not the facts.

Built with the [c0mpute example](https://github.com/profullstack/c0mpute/tree/master/examples/business-strategist-agent).
"""


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("adapter_dir", help="out/<name>; the GGUF is read from out/<name>-gguf")
    ap.add_argument("--repo", required=True, help="<user or org>/<name>; uploads to <repo>-GGUF")
    ap.add_argument("--public", action="store_true")
    args = ap.parse_args()

    token = os.environ.get("HF_TOKEN")  # falls back to `hf auth login` when unset
    d = Path(args.adapter_dir)
    m = json.loads((d / "manifest.json").read_text())
    g = d.parent / f"{d.name}-gguf"
    gguf = next(g.glob("*.gguf"))
    (g / "README.md").write_text(card(m, args.repo, gguf))
    (g / "system").write_text(m["system_prompt"])
    (g / "params").write_text(json.dumps(PARAMS))

    api = HfApi(token=token)
    repo = f"{args.repo}-GGUF"
    api.create_repo(repo, private=not args.public, exist_ok=True)
    api.upload_folder(repo_id=repo, folder_path=g, allow_patterns=["*.gguf", "README.md", "system", "params"],
                      commit_message=f"{m['name']}: {gguf.name}")
    print(f"https://huggingface.co/{repo} ({'public' if args.public else 'private'})")


if __name__ == "__main__":
    main()
