#!/usr/bin/env python3
"""Tests for the video runner's job contract.

Dependency-free on purpose: `python3 test_runner.py` from this directory is
the whole command, in CI and on a provider's box. The generation path needs a
GPU and is not covered here; everything that decides whether a job is accepted
is, because that is what makes a worker win an auction it cannot finish.
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
# In a checkout the manifest is the parent directory's module.toml; in the
# runner image everything sits side by side and the env var points at it.
MANIFEST = Path(os.environ.get("C0MPUTE_VIDEO_MODULE_TOML") or HERE.parent / "module.toml")
os.environ["C0MPUTE_VIDEO_MODULE_TOML"] = str(MANIFEST)
sys.path.insert(0, str(HERE))

import runner  # noqa: E402

FAILURES: list[str] = []


def check(name: str, cond: bool, detail: str = "") -> None:
    if cond:
        print(f"  ok   {name}")
    else:
        print(f"  FAIL {name} {detail}")
        FAILURES.append(name)


def refuses(name: str, raw: dict, job_dir: Path, expect: str = "") -> None:
    """A job the runner must refuse, with the reason mentioning `expect`."""
    models = runner.load_models(MANIFEST)
    try:
        runner.parse_job(raw, models, job_dir)
    except runner.JobError as exc:
        check(name, expect.lower() in str(exc).lower(), f"(got {exc!r})")
        return
    check(name, False, "(accepted, should have been refused)")


def base_job(**over) -> dict:
    job = {
        "job_id": "11111111-1111-1111-1111-111111111111",
        "workload": "video.txt2vid",
        "model": "wan2.2-ti2v-5b",
        "prompt": "a neon alley in the rain",
        "frames": 81,
        "fps": 24,
        "steps": 30,
        "width": 1280,
        "height": 704,
        "seed": 5,
    }
    job.update(over)
    return job


def main() -> int:
    tmp = Path(tempfile.mkdtemp(prefix="video-runner-test-"))

    print("manifest is the single source of models")
    models = runner.load_models(MANIFEST)
    check("three models load", len(models) == 3, f"(got {sorted(models)})")
    check("default model present", "wan2.2-ti2v-5b" in models)
    for mid, model in models.items():
        check(f"{mid} has a diffusers repo", model.diffusers_repo.endswith("-Diffusers"))
        check(f"{mid} declares workloads", len(model.workloads) > 0)
        check(f"{mid} has a vram floor", model.vram_gib > 0)

    print("a well-formed job is accepted and defaulted")
    job = runner.parse_job(base_job(), models, tmp)
    check("model resolved", job["model"].id == "wan2.2-ti2v-5b")
    check("seed kept", job["seed"] == 5)
    check("guidance default", job["guidance_scale"] == 5.0)
    no_seed = runner.parse_job(base_job(seed=None), models, tmp)
    check("absent seed is generated", isinstance(no_seed["seed"], int))

    print("bad jobs are refused before anything loads")
    refuses("unknown workload", base_job(workload="video.gif"), tmp, "unsupported workload")
    refuses("unknown model", base_job(model="sora-2"), tmp, "unknown model")
    refuses("empty prompt", base_job(prompt="   "), tmp, "prompt")
    refuses("overlong prompt", base_job(prompt="x" * 3000), tmp, "exceeds")
    refuses("too many frames", base_job(frames=5000), tmp, "frames must be")
    refuses("zero frames", base_job(frames=0), tmp, "frames must be")
    refuses("too many steps", base_job(steps=500), tmp, "steps must be")
    refuses("bad fps", base_job(fps=0), tmp, "fps must be")
    refuses("non-multiple-of-16 size", base_job(width=1281), tmp, "multiples of 16")
    refuses("oversized frame", base_job(width=1920, height=1088), tmp, "exceed")

    print("a model only serves the workloads it declares")
    # t2v-a14b is text-to-video only, so an img2vid job naming it is refused
    # before the missing-image check can mask the real reason.
    refuses(
        "t2v model refuses img2vid",
        base_job(workload="video.img2vid", model="wan2.2-t2v-a14b", image="in.png"),
        tmp,
        "does not serve",
    )

    print("img2vid guards the conditioning image")
    refuses("img2vid without an image", base_job(workload="video.img2vid"), tmp, "needs an")
    refuses(
        "img2vid with a path",
        base_job(workload="video.img2vid", image="../../etc/passwd"),
        tmp,
        "bare filename",
    )
    refuses(
        "img2vid with a missing file",
        base_job(workload="video.img2vid", image="nope.png"),
        tmp,
        "not found",
    )
    (tmp / "in.png").write_bytes(b"\x89PNG\r\n\x1a\n")
    ok = runner.parse_job(
        base_job(workload="video.img2vid", model="wan2.2-i2v-a14b", image="in.png"), models, tmp
    )
    check("img2vid accepts a present image", ok["image"] == tmp / "in.png")

    print("validate() rejects output that does not match the job")
    probed = {
        "codec": "h264",
        "width": 1280,
        "height": 704,
        "frames": 81,
        "duration_s": 3.4,
        "size_bytes": 1000,
    }
    try:
        runner.validate(probed, job)
        check("matching output passes", True)
    except runner.JobError as exc:
        check("matching output passes", False, f"(got {exc!r})")

    for name, bad in [
            ("wrong dimensions", {**probed, "width": 640}),
            ("wrong frame count", {**probed, "frames": 12}),
            ("zero duration", {**probed, "duration_s": 0}),
            ("oversized result", {**probed, "size_bytes": runner.MAX_OUTPUT_BYTES + 1}),
    ]:
        try:
            runner.validate(bad, job)
            check(f"rejects {name}", False, "(accepted)")
        except runner.JobError:
            check(f"rejects {name}", True)

    print("the selftest runs end to end")
    sel = tmp / "selftest"
    rc = runner.selftest(sel)
    check("selftest returns 0", rc == 0)
    check("selftest wrote a clip", (sel / "out.mp4").stat().st_size > 0)
    sidecar = json.loads((sel / "out.json").read_text())
    check("sidecar names the model", sidecar["model"] == "wan2.2-ti2v-5b")
    check("sidecar records what was produced", sidecar["produced"]["codec"] == "h264")
    check("frames are cleaned up", not (sel / "frames").exists())

    print()
    if FAILURES:
        print(f"{len(FAILURES)} failed: {', '.join(FAILURES)}")
        return 1
    print("all runner contract tests passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
