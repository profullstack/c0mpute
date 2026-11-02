#!/usr/bin/env python3
"""Publish a trained adapter to the Hugging Face Hub (free for public and private repos).

    HF_TOKEN=hf_... .venv/bin/python publish.py out/saas-business-strategist-agent --repo profullstack/saas-business-strategist-agent
    HF_TOKEN=hf_... .venv/bin/python publish.py out/saas-business-strategist-agent --repo profullstack/saas-business-strategist-agent --public

Needs a token with write access (huggingface.co/settings/tokens). Private by
default: the adapter was trained on other publishers' posts, so making it
public is a deliberate choice (--public), not the default.
Uploads the adapter, tokenizer, manifest.json and a generated model card. The
corpus itself is never uploaded.
"""

import argparse
import json
import os
from pathlib import Path

from huggingface_hub import HfApi


def model_card(m, repo):
    hp = m.get("hyperparams", {})
    return f"""---
base_model: {m['base_model']}
library_name: peft
pipeline_tag: text-generation
tags: [lora, peft, business, strategy, cpu, c0mpute]
---

# {m['name']}

A LoRA adapter for [{m['base_model']}](https://huggingface.co/{m['base_model']}) that answers
in the register of independent business writing. Trained on CPU with the
[c0mpute business-strategist-agent example](https://github.com/profullstack/c0mpute/tree/master/examples/business-strategist-agent).

- Training data: {m['documents']} posts harvested from `{', '.join(m.get('sources') or [m['dataset']])}` (rssamplifier.com topic feeds; not redistributed here)
- Steps: {m['steps']}, LoRA r={hp.get('lora_r')}, seq_len={hp.get('seq_len')}, lr={hp.get('lr')}, dtype={hp.get('dtype')}
- Adapter sha256: `{m['adapter_sha256']}`

## Use

```python
from peft import PeftModel
from transformers import AutoModelForCausalLM, AutoTokenizer

tok = AutoTokenizer.from_pretrained("{repo}")
model = PeftModel.from_pretrained(AutoModelForCausalLM.from_pretrained("{m['base_model']}"), "{repo}")
msgs = [{{"role": "system", "content": {json.dumps(m['system_prompt'])}}},
        {{"role": "user", "content": "How should a 3-person SaaS price its first plan?"}}]
ids = tok.apply_chat_template(msgs, add_generation_prompt=True, return_tensors="pt")
print(tok.decode(model.generate(ids, max_new_tokens=300)[0][ids.shape[1]:], skip_special_tokens=True))
```

Best results come from putting fresh search results in the prompt (the example's `chat.py`
uses pit.world search): a sub-1B model supplies the voice, not the facts.
"""


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("model_dir")
    ap.add_argument("--repo", required=True, help="<user or org>/<name> on huggingface.co")
    ap.add_argument("--public", action="store_true", help="make the repo public (default private)")
    args = ap.parse_args()

    token = os.environ.get("HF_TOKEN")
    if not token:
        raise SystemExit("set HF_TOKEN to a write token from https://huggingface.co/settings/tokens")
    d = Path(args.model_dir)
    m = json.loads((d / "manifest.json").read_text())
    (d / "README.md").write_text(model_card(m, args.repo))

    api = HfApi(token=token)
    api.create_repo(args.repo, private=not args.public, exist_ok=True)
    api.upload_folder(
        repo_id=args.repo,
        folder_path=d,
        allow_patterns=["*.safetensors", "*.json", "*.jinja", "*.txt", "README.md", "merges.txt", "vocab.json"],
        ignore_patterns=["resume.pt", "*.tmp"],
        commit_message=f"{m['name']}: {m['steps']} steps on {m['documents']} posts",
    )
    print(f"https://huggingface.co/{args.repo} ({'public' if args.public else 'private'})")


if __name__ == "__main__":
    main()
