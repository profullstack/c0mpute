#!/usr/bin/env python3
"""One ACE-Step 1.5 song for a c0mpute `music.txt2music` job.

    music_worker.py <root> <request.json> <out_dir>

Prints one JSON line on success: {"path": ..., "format": ..., "secs": ...}.
Anything else (a non-zero exit, no line) is a failed job. The node owns all
validation; this script trusts its request and only turns it into audio.

Runs DiT-only (no 5 Hz language model): on CPU the LM roughly doubles memory
and time, and the turbo DiT follows the caption and lyrics on its own.
"""

import json
import os
import shutil
import sys
import time


def main() -> int:
    root, request_path, out_dir = sys.argv[1], sys.argv[2], sys.argv[3]
    with open(request_path, encoding="utf-8") as f:
        req = json.load(f)

    # Never reach the network at job time: the node pre-downloaded the weights.
    os.environ.setdefault("HF_HUB_OFFLINE", "1")
    os.environ.setdefault("TRANSFORMERS_OFFLINE", "1")
    os.environ.setdefault("HF_HUB_DISABLE_TELEMETRY", "1")
    os.environ.setdefault("ACESTEP_DOWNLOAD_SOURCE", "huggingface")
    os.environ.setdefault("ACESTEP_DISABLE_TQDM", "1")
    os.environ.pop("SPACE_ID", None)

    import torch

    threads = int(req.get("threads") or 0)
    if threads > 0:
        torch.set_num_threads(threads)

    from acestep.handler import AceStepHandler
    from acestep.inference import GenerationConfig, GenerationParams, generate_music

    started = time.monotonic()
    dit = AceStepHandler()

    # ACE-Step loads in float32 on CPU. bf16 halves the memory (~7 GB instead
    # of ~14 GB) and is fast on CPUs with AVX512-BF16/AMX. The dtype is set
    # just before the model check, and the weights load after it, so switching
    # it there means the fp32 copy never exists.
    want_bf16 = req.get("dtype", "bfloat16") == "bfloat16"
    original_check = dit._ensure_models_present

    def check_then_pick_dtype(*args, **kwargs):
        if want_bf16 and getattr(dit, "device", "cpu") == "cpu":
            dit.dtype = torch.bfloat16
        return original_check(*args, **kwargs)

    dit._ensure_models_present = check_then_pick_dtype

    message, ok = dit.initialize_service(
        project_root=root,
        config_path="acestep-v15-turbo",
        device=req.get("device", "auto"),
        offload_to_cpu=False,
        quantization=None,
        prefer_source="huggingface",
    )
    if not ok:
        print(f"initialize_service failed: {message}", file=sys.stderr)
        return 2
    loaded = time.monotonic()

    lyrics = (req.get("lyrics") or "").strip() or "[Instrumental]"
    params = GenerationParams(
        caption=req["prompt"],
        lyrics=lyrics,
        duration=int(req["duration"]),
        inference_steps=int(req.get("steps") or 8),
        thinking=False,
        use_cot_metas=False,
        use_cot_caption=False,
        use_cot_language=False,
    )
    fmt = "mp3" if shutil.which("ffmpeg") else "flac"
    seed = req.get("seed")
    config = GenerationConfig(
        batch_size=1,
        use_random_seed=seed is None,
        seeds=[int(seed)] if seed is not None else None,
        audio_format=fmt,
    )
    result = generate_music(dit, None, params, config, save_dir=out_dir)
    if not result.success or not result.audios:
        print(f"generate_music failed: {result.error}", file=sys.stderr)
        return 3
    path = result.audios[0].get("path")
    if not path or not os.path.isfile(path):
        print("generate_music returned no file", file=sys.stderr)
        return 3
    print(json.dumps({
        "path": path,
        "format": fmt,
        "secs": round(time.monotonic() - started, 1),
        "load_secs": round(loaded - started, 1),
        "dtype": str(dit.dtype).replace("torch.", ""),
        "device": str(dit.device),
    }))
    return 0


if __name__ == "__main__":
    sys.exit(main())
