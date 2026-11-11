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
    check("five models load", len(models) == 5, f"(got {sorted(models)})")
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
    refuses("wan frames not 4k+1", base_job(frames=80), tmp, "4k+1")

    print("the small Wan 2.1 model fits an 8 GiB card and defaults small")
    small = "wan2.1-t2v-1.3b"
    check("wan2.1 1.3b is advertised", small in models)
    check("8 GiB floor", models[small].vram_gib == 8)
    check("text-to-video only", models[small].workloads == ("video.txt2vid",))
    sj = runner.parse_job(
        {"workload": "video.txt2vid", "model": small, "prompt": "a fox in the snow"}, models, tmp
    )
    check(
        "omitted fields take the model's defaults",
        (sj["frames"], sj["fps"], sj["steps"], sj["width"], sj["height"]) == (33, 16, 20, 480, 272),
        f"(got {sj['frames']} {sj['fps']} {sj['steps']} {sj['width']}x{sj['height']})",
    )
    check(
        "explicit fields still win",
        runner.parse_job(base_job(model=small, frames=81), models, tmp)["frames"] == 81,
    )
    check(
        "other models keep the old defaults",
        runner.parse_job(
            {"workload": "video.txt2vid", "model": "wan2.2-ti2v-5b", "prompt": "p"}, models, tmp
        )["frames"] == 81,
    )
    refuses(
        "wan2.1 refuses img2vid",
        base_job(workload="video.img2vid", model=small, image="in.png"),
        tmp,
        "does not serve",
    )

    print("offload only when the card cannot hold the whole pipeline")
    check("8 GiB card offloads wan2.1", runner.use_offload(models[small], 8.0))
    check("a card reporting 7.6 GiB offloads too", runner.use_offload(models[small], 7.6))
    check("a 24 GiB card loads it whole", not runner.use_offload(models[small], 24.0))
    check(
        "models without full_vram_gib never offload",
        not runner.use_offload(models["wan2.2-t2v-a14b"], 8.0),
    )
    # The 5B pipeline is ~24.2 GB in fp16/bf16 + fp32 VAE: a 24 GiB card
    # (RTX 3090/4090) cannot hold it whole, and under WSL (Salad) the driver
    # spills to system memory instead of OOMing, so the clip crawls at 0 steps.
    check("a 24 GiB card offloads wan2.2 5B", runner.use_offload(models["wan2.2-ti2v-5b"], 23.6))
    check("a 32 GiB card loads wan2.2 5B whole", not runner.use_offload(models["wan2.2-ti2v-5b"], 31.4))

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

    print("LTX-2 (video + audio) has its own rules")
    av = "ltx2.3-av-distilled"
    check("audio model is ltx2", models[av].backend == "ltx2" and models[av].audio)
    check("wan rows default to the wan backend", models["wan2.2-ti2v-5b"].backend == "wan")
    ltx = runner.parse_job(base_job(model=av), models, tmp)
    check("node defaults (81f, 1280x704) are accepted", ltx["frames"] == 81)
    check("distilled runs its fixed 8 steps", ltx["steps"] == runner.LTX2_DISTILLED_STEPS)
    check("distilled defaults CFG off", ltx["guidance_scale"] == 1.0)
    check(
        "explicit guidance is kept",
        runner.parse_job(base_job(model=av, guidance_scale=2.0), models, tmp)["guidance_scale"] == 2.0,
    )
    refuses("ltx2 frames not 8k+1", base_job(model=av, frames=80), tmp, "8k+1")
    refuses("ltx2 size not /32", base_job(model=av, width=1264), tmp, "multiples of 32")

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

    print("an audio model never returns a silent clip")
    av_job = runner.parse_job(base_job(model=av), models, tmp)
    try:
        runner.validate({**probed, "audio": None}, av_job)
        check("silent clip from audio model rejected", False, "(accepted)")
    except runner.JobError as exc:
        check("silent clip from audio model rejected", "soundtrack" in str(exc))
    try:
        runner.validate(
            {**probed, "audio": {"codec": "aac", "sample_rate": 48000, "channels": 2}}, av_job
        )
        check("clip with audio passes", True)
    except runner.JobError as exc:
        check("clip with audio passes", False, f"(got {exc!r})")

    print("write_wav makes a valid 16-bit WAV")
    import wave

    wav = tmp / "t.wav"
    runner.write_wav([[0.0, 0.5, -0.5, 2.0], [0.0, -1.0, 1.0, -2.0]], 48000, wav)
    with wave.open(str(wav)) as w:
        check("wav header", (w.getnchannels(), w.getsampwidth(), w.getframerate(), w.getnframes()) == (2, 2, 48000, 4))
    runner.write_wav([0.1, 0.2], 24000, wav)
    with wave.open(str(wav)) as w:
        check("mono 1-D input", w.getnchannels() == 1 and w.getnframes() == 2)
    for name, bad in [("empty", []), ("ragged", [[0.1, 0.2], [0.1]])]:
        try:
            runner.write_wav(bad, 48000, wav)
            check(f"wav refuses {name}", False, "(accepted)")
        except runner.JobError:
            check(f"wav refuses {name}", True)

    print("adult LoRAs load on adult jobs only")
    models = runner.load_models(MANIFEST)
    job_dir = tmp / "adult"
    job_dir.mkdir(exist_ok=True)
    check("adult flag parsed", runner.parse_job(base_job(adult=True), models, job_dir)["adult"] is True)
    check("adult defaults to false", runner.parse_job(base_job(), models, job_dir)["adult"] is False)
    check("adult must be literally true", runner.parse_job(base_job(adult="yes"), models, job_dir)["adult"] is False)
    rows = runner.parse_adult_loras("m", [{"repo": "o/r", "file": "x.safetensors", "scale": 0.8, "trigger": "t", "match": ["Alpha"], "default": True}])
    check("adult_loras parse", rows == (runner.AdultLora("o/r", "x.safetensors", 0.8, "t", ("alpha",), True),), str(rows))
    for label, bad in [("no repo", [{"file": "x.safetensors"}]), ("not safetensors", [{"repo": "o/r", "file": "x.ckpt"}]),
                       ("scale 0", [{"repo": "o/r", "file": "x.safetensors", "scale": 0}]),
                       ("two defaults", [{"repo": "a/b", "file": "a.safetensors", "default": True}, {"repo": "c/d", "file": "c.safetensors", "default": True}])]:
        try:
            runner.parse_adult_loras("m", bad)
            check(f"adult_loras refuses {label}", False, "(accepted)")
        except runner.JobError:
            check(f"adult_loras refuses {label}", True)

    A = runner.AdultLora("o/a", "a.safetensors", 0.9, "atrig", ("alpha", "alpha beta"), True)
    B = runner.AdultLora("o/b", "b.safetensors", 0.7, "btrig", ("gamma",), False)
    check("picks the matching LoRA", runner.pick_adult_lora((A, B), "a Gamma scene") is B)
    check("falls back to the default", runner.pick_adult_lora((A, B), "nothing matches") is A)
    check("whole words only", runner.pick_adult_lora((B,), "gammas everywhere") is None)

    class FakePipe:
        def __init__(self):
            self.calls = []
        def load_lora_weights(self, repo, weight_name, adapter_name):
            self.calls.append(("load", repo, weight_name, adapter_name))
        def set_adapters(self, names, adapter_weights):
            self.calls.append(("set", tuple(names), tuple(adapter_weights)))

    m = runner.Model(id="m", repo="", diffusers_repo="d", workloads=("video.txt2vid",), precision="bf16", vram_gib=24, adult_loras=(A, B))
    pipe, job = FakePipe(), {"adult": True, "prompt": "a gamma scene"}
    loaded = runner.apply_adult_loras(pipe, m, job)
    check("adult job loads one LoRA", pipe.calls == [("load", "o/b", "b.safetensors", "adult"), ("set", ("adult",), (0.7,))], str(pipe.calls))
    check("trigger is prepended", job["prompt"] == "btrig, a gamma scene", job["prompt"])
    check("adult job reports what loaded", loaded == ["o/b/b.safetensors@0.7"])
    pipe, job = FakePipe(), {"adult": False, "prompt": "a gamma scene"}
    check("SFW job loads nothing", runner.apply_adult_loras(pipe, m, job) == [] and pipe.calls == [] and job["prompt"] == "a gamma scene")

    print("the selftest runs end to end")
    sel = tmp / "selftest"
    rc = runner.selftest(sel)
    check("selftest returns 0", rc == 0)
    check("selftest wrote a clip", (sel / "out.mp4").stat().st_size > 0)
    sidecar = json.loads((sel / "out.json").read_text())
    # The last leg of the selftest is the audio one, so its sidecar wins.
    check("sidecar names the audio model", sidecar["model"] == av)
    check("sidecar records what was produced", sidecar["produced"]["codec"] == "h264")
    check("selftest clip carries aac audio", (sidecar["produced"]["audio"] or {}).get("codec") == "aac")
    check("frames are cleaned up", not (sel / "frames").exists())

    print()
    if FAILURES:
        print(f"{len(FAILURES)} failed: {', '.join(FAILURES)}")
        return 1
    print("all runner contract tests passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
