#!/usr/bin/env python3
"""Make a network-trained adapter publishable.

    c0mpute infernet text train --job job-legal.json --output out/not-a-lawyer-agent --json --yes > data/legal.ndjson
    .venv/bin/python network_manifest.py job-legal.json out/not-a-lawyer-agent data/legal.ndjson

`c0mpute infernet text train` hands back the adapter and a short manifest. This
adds what chat.py, export_gguf.py and publish.py need: the tokenizer and chat
template, the corpus sources, the job's example question, disclaimer and tags,
held-out perplexity before and after, and the c0mpute receipt (job, worker,
output hash), which must match the adapter's sha256.
"""

import argparse
import hashlib
import json
from pathlib import Path

from transformers import AutoTokenizer


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("job")
    ap.add_argument("adapter_dir")
    ap.add_argument("events", help="the buyer's --json output (NDJSON)")
    args = ap.parse_args()

    job = json.loads(Path(args.job).read_text())
    d = Path(args.adapter_dir)
    m = json.loads((d / "manifest.json").read_text())
    events = [json.loads(l) for l in Path(args.events).read_text().splitlines() if l.startswith("{")]

    evals = [e for e in events if e.get("type") == "eval"]
    result = next((e for e in reversed(events) if "receipt" in e), None)
    if not result:
        raise SystemExit(f"{args.events}: no receipt; the network run did not finish")
    receipt = result["receipt"]
    digest = hashlib.sha256((d / "adapter_model.safetensors").read_bytes()).hexdigest()
    if receipt["output_hash"] != f"sha256:{digest}":
        raise SystemExit(f"receipt hash {receipt['output_hash']} != adapter sha256:{digest}")

    corpus = Path(args.job).parent / job["dataset"]["url"]
    sources = []
    for line in corpus.read_text().splitlines():
        s = json.loads(line).get("source")
        if s and s not in sources:
            sources.append(s)

    m.update({
        "dataset": job["dataset"]["url"],
        "sources": sources,
        **{k: job[k] for k in ("example_question", "disclaimer", "tags") if job.get(k)},
        "trained_on": {
            "network": "c0mpute",
            "workload": "infernet.text.train",
            "worker": receipt["worker_peer_id"],
            "job_id": receipt["job_id"],
            "receipt_output_hash": receipt["output_hash"],
        },
    })
    if len(evals) >= 2:
        m["eval"] = {"perplexity_before": round(evals[0]["metrics"]["perplexity"], 2),
                     "perplexity_after": round(evals[-1]["metrics"]["perplexity"], 2)}
    (d / "manifest.json").write_text(json.dumps(m, indent=2))
    AutoTokenizer.from_pretrained(m["base_model"]).save_pretrained(d)
    print(d, f"receipt ok, {len(sources)} sources", m.get("eval", ""))


if __name__ == "__main__":
    main()
