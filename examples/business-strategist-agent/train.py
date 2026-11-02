#!/usr/bin/env python3
"""LoRA-tune a small instruct model on an rssamplifier corpus, on CPU.

    uv run --with-requirements requirements.txt train.py job.json

`job.json` is the job config shape from @infernetprotocol/training
(packages/training/src/protocol.js): kind, base_model, dataset, hyperparams,
output. Progress goes to stdout as training-protocol v1 NDJSON
(meta / step / eval / checkpoint / done), the same events a c0mpute training
backend streams, so this script can stand in as that backend's runner.
Human-readable logs go to stderr.
"""

import hashlib
import json
import math
import os
import random
import sys
import time
import urllib.request
import uuid
from datetime import datetime, timezone
from pathlib import Path

import torch
from peft import LoraConfig, get_peft_model
from transformers import AutoModelForCausalLM, AutoTokenizer

SYSTEM = (
    "You are business-strategist-agent. You give concise, practical business "
    "strategy grounded in what independent business writers are publishing."
)


def now():
    return datetime.now(timezone.utc).isoformat()


def emit(job_id, type_, **fields):
    print(json.dumps({"v": 1, "type": type_, "id": job_id, **fields}), flush=True)


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def load_dataset(spec, base_dir):
    url = spec["url"]
    if url.startswith(("http://", "https://")):
        raw = urllib.request.urlopen(url, timeout=120).read()
    else:
        raw = (base_dir / url.removeprefix("file://")).read_bytes()
    if spec.get("sha256") and hashlib.sha256(raw).hexdigest() != spec["sha256"]:
        raise SystemExit("dataset sha256 mismatch")
    return [json.loads(line) for line in raw.decode().splitlines() if line.strip()]


def to_chat(tok, doc):
    # Each post becomes one briefing exchange, so the tune keeps the instruct
    # model's chat format instead of drifting into raw blog prose.
    return tok.apply_chat_template(
        [
            {"role": "system", "content": SYSTEM},
            {"role": "user", "content": f"Brief me on: {doc['title']}"},
            {"role": "assistant", "content": doc["text"]},
        ],
        tokenize=False,
    )


def pack(tok, docs, seq_len):
    ids = []
    for d in docs:
        ids.extend(tok(to_chat(tok, d), add_special_tokens=False)["input_ids"])
    blocks = [ids[i : i + seq_len] for i in range(0, len(ids) - seq_len + 1, seq_len)]
    return torch.tensor(blocks, dtype=torch.long)


@torch.no_grad()
def perplexity(model, blocks, batch_size):
    model.eval()
    losses = [
        model(input_ids=blocks[i : i + batch_size], labels=blocks[i : i + batch_size]).loss.item()
        for i in range(0, len(blocks), batch_size)
    ]
    model.train()
    return math.exp(sum(losses) / max(len(losses), 1))


def main():
    job_path = Path(sys.argv[1] if len(sys.argv) > 1 else "job.json")
    job = json.loads(job_path.read_text())
    hp = {
        "lr": 2e-4, "batch_size": 4, "epochs": 1, "max_steps": None,
        "seq_len": 256, "lora_r": 8, "threads": min(4, os.cpu_count()), "seed": 0,
        **job.get("hyperparams", {}),
    }
    job_id = job.get("id") or str(uuid.uuid4())
    out_dir = job_path.parent / job["output"]["destination_url"].removeprefix("file://")

    random.seed(hp["seed"])
    torch.manual_seed(hp["seed"])
    torch.set_num_threads(hp["threads"])

    emit(job_id, "meta", started_at=now(), backend="local-cpu", world_size=1,
         kind=job["kind"], base_model=job["base_model"])

    docs = load_dataset(job["dataset"], job_path.parent)
    random.shuffle(docs)
    held = max(1, len(docs) // 20)
    tok = AutoTokenizer.from_pretrained(job["base_model"])
    train_blocks = pack(tok, docs[held:], hp["seq_len"])
    eval_blocks = pack(tok, docs[:held], hp["seq_len"])
    log(f"{len(docs)} docs -> {len(train_blocks)} train / {len(eval_blocks)} eval blocks of {hp['seq_len']} tokens")

    model = AutoModelForCausalLM.from_pretrained(job["base_model"], dtype=torch.float32)
    # Recompute activations in the backward pass: roughly halves peak RAM, so
    # the job fits beside other work on a shared box (earlyoom killed it at 3.2 GB).
    model.gradient_checkpointing_enable()
    model.enable_input_require_grads()
    model = get_peft_model(model, LoraConfig(
        r=hp["lora_r"], lora_alpha=hp["lora_r"] * 2, lora_dropout=0.05,
        target_modules=["q_proj", "k_proj", "v_proj", "o_proj"], task_type="CAUSAL_LM",
    ))
    trainable = sum(p.numel() for p in model.parameters() if p.requires_grad)
    log(f"trainable params: {trainable:,}")

    steps_per_epoch = math.ceil(len(train_blocks) / hp["batch_size"])
    total = hp["max_steps"] or steps_per_epoch * hp["epochs"]
    opt = torch.optim.AdamW([p for p in model.parameters() if p.requires_grad], lr=hp["lr"])
    sched = torch.optim.lr_scheduler.LambdaLR(opt, lambda s: min(1.0, (s + 1) / 10) * (1 - s / total))

    if len(eval_blocks):
        emit(job_id, "eval", step=0, metrics={"perplexity": perplexity(model, eval_blocks, hp["batch_size"])})

    step, t0 = 0, time.time()
    model.train()
    while step < total:
        order = torch.randperm(len(train_blocks))
        for i in range(0, len(order), hp["batch_size"]):
            batch = train_blocks[order[i : i + hp["batch_size"]]]
            loss = model(input_ids=batch, labels=batch).loss
            loss.backward()
            grad_norm = torch.nn.utils.clip_grad_norm_(model.parameters(), 1.0).item()
            opt.step(); sched.step(); opt.zero_grad()
            step += 1
            emit(job_id, "step", step=step, loss=round(loss.item(), 4), lr=sched.get_last_lr()[0],
                 grad_norm=round(grad_norm, 3),
                 throughput=round(step * hp["batch_size"] * hp["seq_len"] / (time.time() - t0), 1))
            if step >= total:
                break

    if len(eval_blocks):
        emit(job_id, "eval", step=step, metrics={"perplexity": perplexity(model, eval_blocks, hp["batch_size"])})

    out_dir.mkdir(parents=True, exist_ok=True)
    model.save_pretrained(out_dir)
    tok.save_pretrained(out_dir)
    weights = out_dir / "adapter_model.safetensors"
    digest = hashlib.sha256(weights.read_bytes()).hexdigest()
    (out_dir / "manifest.json").write_text(json.dumps({
        "name": job.get("name", "business-strategist-agent"),
        "base_model": job["base_model"],
        "adapter_sha256": digest,
        "dataset": job["dataset"]["url"],
        "documents": len(docs),
        "steps": step,
        "system_prompt": SYSTEM,
    }, indent=2))
    emit(job_id, "checkpoint", step=step, path=str(out_dir), size_bytes=weights.stat().st_size,
         model=f"sha256:{digest}")
    emit(job_id, "done", reason="complete", final_step=step, finished_at=now())


if __name__ == "__main__":
    main()
