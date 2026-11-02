#!/usr/bin/env python3
"""Merge a trained adapter into its base model and write a GGUF for Ollama / llama.cpp.

    git clone --depth 1 https://github.com/ggml-org/llama.cpp ../llama.cpp
    uv pip install -p .venv -e ../llama.cpp/gguf-py sentencepiece
    .venv/bin/python export_gguf.py out/saas-business-strategist-agent --llama-cpp ../llama.cpp
    ollama create saas-business-strategist-agent -f out/saas-business-strategist-agent-gguf/Modelfile

Ollama only takes safetensors LoRA adapters for Llama, Mistral and Gemma bases, so a
Qwen adapter is merged into the base weights first. Output: <adapter>-gguf/ with the
GGUF (Q8_0 by default) and a Modelfile carrying the agent's system prompt.
"""

import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

import torch
from peft import PeftModel
from transformers import AutoModelForCausalLM, AutoTokenizer


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("adapter")
    ap.add_argument("--llama-cpp", required=True, help="a llama.cpp checkout (for convert_hf_to_gguf.py)")
    ap.add_argument("--outtype", default="q8_0", help="q8_0, f16, bf16 (default q8_0)")
    args = ap.parse_args()

    adapter = Path(args.adapter)
    m = json.loads((adapter / "manifest.json").read_text())
    out_dir = adapter.with_name(adapter.name + "-gguf")
    out_dir.mkdir(exist_ok=True)
    gguf = out_dir / f"{m['name']}-{args.outtype.upper()}.gguf"
    torch.set_num_threads(4)

    with tempfile.TemporaryDirectory(dir=out_dir) as merged:
        base = AutoModelForCausalLM.from_pretrained(m["base_model"], dtype=torch.float16)
        PeftModel.from_pretrained(base, adapter).merge_and_unload().save_pretrained(merged, safe_serialization=True)
        AutoTokenizer.from_pretrained(adapter).save_pretrained(merged)
        subprocess.run(
            [sys.executable, str(Path(args.llama_cpp) / "convert_hf_to_gguf.py"), merged,
             "--outtype", args.outtype, "--outfile", str(gguf)],
            check=True,
        )

    # The GGUF carries Qwen's chat template; the Modelfile adds the agent's system prompt.
    (out_dir / "Modelfile").write_text(
        f"FROM ./{gguf.name}\n"
        f"SYSTEM {json.dumps(m['system_prompt'])}\n"
        "PARAMETER temperature 0.3\n"
        "PARAMETER repeat_penalty 1.1\n"
    )
    print(gguf, f"{gguf.stat().st_size / 1e6:.0f} MB")


if __name__ == "__main__":
    main()
