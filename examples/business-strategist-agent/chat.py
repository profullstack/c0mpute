#!/usr/bin/env python3
"""Ask business-strategist-agent a question, on CPU.

    .venv/bin/python chat.py --model out/saas-business-strategist-agent "How should a 3-person SaaS price its first plan?"

A 135M model cannot memorise thousands of posts, so the tune teaches it the
register and search supplies the facts: the question goes to pit.world search
(https://pit.world/api/search) and the top pages go into the prompt, cited by
URL. The harvested corpus (data/corpus.jsonl, BM25) is the offline fallback:
it holds one topic's posts, so most questions matched a stray word in it.
"""

import argparse
import json
import math
import re
import sys
import urllib.parse
import urllib.request
from collections import Counter
from pathlib import Path

import torch
from peft import PeftModel
from transformers import AutoModelForCausalLM, AutoTokenizer

HERE = Path(__file__).parent
TOP_K = 3
SEARCH = "https://pit.world/api/search"


def pit_search(query, k=TOP_K):
    """Top web results from pit.world search as {title, url, text}; [] if unreachable."""
    url = f"{SEARCH}?{urllib.parse.urlencode({'q': query})}"
    req = urllib.request.Request(url, headers={"accept": "application/json", "user-agent": "c0mpute-bsa/1.0"})
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            body = json.load(r)
    except Exception as e:  # offline, timeout, bad JSON: fall back to the corpus
        print(f"(pit.world search unavailable: {e}; using the local corpus)", file=sys.stderr)
        return []
    hits = []
    for item in body.get("results") or []:
        if not item.get("url") or not item.get("title"):
            continue
        hits.append({"title": item["title"], "url": item["url"], "text": item.get("description") or ""})
        if len(hits) >= k:
            break
    return hits

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
    ranked = sorted(scored, key=lambda x: -x[0])[:TOP_K]
    # Drop weak matches: a post scoring under half the best one is usually a
    # stray word overlap (an OS review matching "my"), and it misleads the model.
    best = ranked[0][0] if ranked else 0
    return [d for s, d in ranked if s > 0 and s >= best * 0.5]


def main():
    ap = argparse.ArgumentParser(description="Ask a trained strategist agent a question.")
    ap.add_argument("question", nargs="*")
    ap.add_argument("--model", default="out/business-strategist-agent", help="trained adapter dir")
    args = ap.parse_args()
    question = " ".join(args.question) or "What should a small business focus on next quarter?"
    ADAPTER = HERE / args.model
    manifest = json.loads((ADAPTER / "manifest.json").read_text())
    CORPUS = HERE / manifest["dataset"]
    torch.set_num_threads(4)

    hits = pit_search(question)
    if not hits and CORPUS.exists():
        docs = [json.loads(l) for l in CORPUS.read_text().splitlines() if l.strip()]
        hits = bm25(question, docs)
    context = "\n\n".join(f"[{i + 1}] {d['title']} ({d['url']})\n{d['text'][:700]}" for i, d in enumerate(hits))

    tok = AutoTokenizer.from_pretrained(ADAPTER)
    model = AutoModelForCausalLM.from_pretrained(manifest["base_model"], dtype=torch.bfloat16)
    model = PeftModel.from_pretrained(model, ADAPTER).eval()

    prompt = tok.apply_chat_template(
        [
            {"role": "system", "content": manifest["system_prompt"]},
            {"role": "user", "content": f"Background from the web:\n{context}\n\nUsing the background where it helps, answer: {question}" if hits else question},
        ],
        tokenize=False,
        add_generation_prompt=True,
    )
    ids = tok(prompt, return_tensors="pt")
    with torch.no_grad():
        # Greedy: sampling on a sub-1B model is what produced echoed questions
        # and rambling; deterministic decoding stays on topic.
        out = model.generate(**ids, max_new_tokens=350, do_sample=False, repetition_penalty=1.1)
    print(tok.decode(out[0][ids["input_ids"].shape[1]:], skip_special_tokens=True).strip())
    for i, d in enumerate(hits):
        print(f"  [{i + 1}] {d['url']}")


if __name__ == "__main__":
    main()
