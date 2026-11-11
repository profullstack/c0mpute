#!/usr/bin/env python3
"""Download the diffusers weights for the named models into HF_HOME.

Repos come from the module.toml shipped in the image, so this warms exactly
what runner.py will load. Progress goes to stdout once a minute.
"""
import os
import sys
import threading
import time
import tomllib
from pathlib import Path

from huggingface_hub import snapshot_download

MANIFEST = Path(os.environ.get("C0MPUTE_VIDEO_MODULE_TOML", "/opt/c0mpute-video/module.toml"))


def du(path: Path) -> int:
    total = 0
    for root, _, files in os.walk(path):
        for f in files:
            try:
                total += os.path.getsize(os.path.join(root, f))
            except OSError:
                pass
    return total


def main(ids: list[str]) -> int:
    models = tomllib.loads(MANIFEST.read_text())["module"]["models"]
    hf = Path(os.environ.get("HF_HOME", "/weights/hf"))
    done = threading.Event()

    def report() -> None:
        start = time.time()
        while not done.wait(60):
            gb = du(hf) / 1e9
            print(f"prefetch: {gb:.1f} GB after {int(time.time() - start)} s", flush=True)

    threading.Thread(target=report, daemon=True).start()
    rc = 0
    for mid in ids:
        row = models.get(mid)
        if not row:
            print(f"prefetch: unknown model {mid}", flush=True)
            rc = 1
            continue
        repo = row.get("diffusers_repo") or row["repo"]
        t = time.time()
        snapshot_download(repo, max_workers=16)
        print(f"prefetch: {mid} ({repo}) ready in {int(time.time() - t)} s, cache {du(hf) / 1e9:.1f} GB", flush=True)
    done.set()
    return rc


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
