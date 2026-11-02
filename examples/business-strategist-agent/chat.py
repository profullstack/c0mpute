#!/usr/bin/env python3
"""Ask business-strategist-agent a question, on CPU.

    uv run --with-requirements requirements.txt chat.py "How should a 3-person SaaS price its first plan?"

A 135M model cannot memorise thousands of posts, so the tune teaches it the
register and the corpus supplies the facts: the question is matched against
data/corpus.jsonl with BM25 and the best posts go into the prompt, cited by URL.
"""

import json
import math
import re
import sys
from collections import Counter
from pathlib import Path

import torch
from peft import PeftModel
from transformers import AutoModelForCausalLM, AutoTokenizer

HERE = Path(__file__).parent
ADAPTER = HERE / "out" / "business-strategist-agent"
CORPUS = HERE / "data" / "corpus.jsonl"
TOP_K = 3

words = lambda s: re.findall(r"[a-z0-9']+", s.lower())


def bm25(query, docs, k1=1.5, b=0.75):
    toks = [words(d["title"] + " " + d["text"]) for d in docs]
    avg = sum(map(len, toks)) / len(toks)
    df = Counter(w for t in toks for w in set(t))
    n = len(docs)
    scored = []
    for d, t in zip(docs, toks):
        tf = Counter(t)
        s = sum(
            math.log(1 + (n - df[w] + 0.5) / (df[w] + 0.5))
            * tf[w] * (k1 + 1) / (tf[w] + k1 * (1 - b + b * len(t) / avg))
            for w in set(words(query)) if w in tf
        )
        scored.append((s, d))
    return [d for s, d in sorted(scored, key=lambda x: -x[0])[:TOP_K] if s > 0]


def main():
    question = " ".join(sys.argv[1:]) or "What should a small business focus on next quarter?"
    manifest = json.loads((ADAPTER / "manifest.json").read_text())
    torch.set_num_threads(4)

    docs = [json.loads(l) for l in CORPUS.read_text().splitlines() if l.strip()]
    hits = bm25(question, docs)
    context = "\n\n".join(f"[{i + 1}] {d['title']} ({d['url']})\n{d['text'][:700]}" for i, d in enumerate(hits))

    tok = AutoTokenizer.from_pretrained(ADAPTER)
    model = AutoModelForCausalLM.from_pretrained(manifest["base_model"], dtype=torch.float32)
    model = PeftModel.from_pretrained(model, ADAPTER).eval()

    prompt = tok.apply_chat_template(
        [
            {"role": "system", "content": manifest["system_prompt"]},
            {"role": "user", "content": f"Recent posts:\n{context}\n\nQuestion: {question}" if hits else question},
        ],
        tokenize=False,
        add_generation_prompt=True,
    )
    ids = tok(prompt, return_tensors="pt")
    with torch.no_grad():
        out = model.generate(**ids, max_new_tokens=250, do_sample=True, temperature=0.4,
                             top_p=0.9, repetition_penalty=1.15)
    print(tok.decode(out[0][ids["input_ids"].shape[1]:], skip_special_tokens=True).strip())
    for i, d in enumerate(hits):
        print(f"  [{i + 1}] {d['url']}")


if __name__ == "__main__":
    main()
