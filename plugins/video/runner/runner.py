#!/usr/bin/env python3
"""c0mpute video-generation runner.

One job per process: read a job JSON, generate a clip with Wan 2.2, encode an
MP4, write a metadata sidecar, and exit non-zero on any failure. The node's
`video` runner invokes this inside the container and never parses stdout, so
the contract is entirely files on disk:

    /job/job.json   in    the job (schema below)
    /job/<image>    in    img2vid only: the conditioning frame
    /job/out.mp4    out   the clip
    /job/out.json   out   sidecar: what actually ran

Job JSON:

    {
      "job_id":   "uuid",              # informational, echoed in the sidecar
      "workload": "video.txt2vid",     # or video.img2vid
      "model":    "wan2.2-ti2v-5b",    # an id from module.toml [module.models]
      "prompt":   "a neon alley in the rain",
      "negative_prompt": "",           # optional
      "image":    "input.png",         # img2vid only, relative to the job dir
      "frames":   81,                  # optional, defaults from module.toml
      "fps":      24,
      "steps":    30,
      "width":    1280,
      "height":   704,
      "seed":     12345,               # optional; omitted means random
      "guidance_scale": 5.0            # optional
    }

The model table is read from module.toml rather than duplicated here, so a
model this runner accepts and a model the marketplace advertises cannot drift
apart. `--selftest` exercises everything except the model itself (encode,
validate, sidecar) and needs neither a GPU nor weights, which is what CI and a
provider's `c0mpute video check` run.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import shutil
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

RUNNER_VERSION = "1"

# Where the image puts the manifest it was built from. Overridable so the
# selftest can run against a checkout.
MODULE_TOML = Path(os.environ.get("C0MPUTE_VIDEO_MODULE_TOML", "/opt/c0mpute-video/module.toml"))

# Caps. A job that asks for more than this is refused before anything is
# loaded: a worker that accepts an unbounded clip length wins the auction and
# then blows its deadline, which costs it the receipt.
MAX_FRAMES = 241
MAX_STEPS = 60
MAX_PIXELS = 1280 * 720
MAX_PROMPT_CHARS = 2000
MAX_OUTPUT_BYTES = 256 * 1024 * 1024


class JobError(Exception):
    """A problem with the job itself — refuse it, do not retry."""


@dataclass(frozen=True)
class Model:
    id: str
    repo: str
    diffusers_repo: str
    workloads: tuple[str, ...]
    precision: str
    vram_gib: int


def load_models(path: Path) -> dict[str, Model]:
    """Read [module.models] out of the plugin manifest."""
    try:
        manifest = tomllib.loads(path.read_text())
    except FileNotFoundError as exc:
        raise JobError(f"model manifest missing at {path}") from exc
    except tomllib.TOMLDecodeError as exc:
        raise JobError(f"model manifest is not valid TOML: {exc}") from exc

    table = manifest.get("module", {}).get("models", {})
    if not table:
        raise JobError(f"{path} declares no [module.models]")

    models: dict[str, Model] = {}
    for mid, row in table.items():
        diffusers_repo = row.get("diffusers_repo")
        if not diffusers_repo:
            # Every model this runner can serve needs a diffusers-format repo.
            # Skipping rather than failing lets the manifest carry a model for
            # some future backend without breaking this one.
            continue
        models[mid] = Model(
            id=mid,
            repo=row.get("repo", ""),
            diffusers_repo=diffusers_repo,
            workloads=tuple(row.get("workloads", ())),
            precision=row.get("precision", "bf16"),
            vram_gib=int(row.get("vram_gib", 0)),
        )
    return models


def parse_job(raw: dict, models: dict[str, Model], job_dir: Path) -> dict:
    """Validate a job and fill in defaults. Raises JobError on anything off."""
    workload = raw.get("workload")
    if workload not in ("video.txt2vid", "video.img2vid"):
        raise JobError(f"unsupported workload {workload!r}")

    model_id = raw.get("model")
    model = models.get(model_id)
    if model is None:
        raise JobError(f"unknown model {model_id!r}; have {sorted(models)}")
    if model.workloads and workload not in model.workloads:
        raise JobError(f"model {model.id} does not serve {workload}")

    prompt = (raw.get("prompt") or "").strip()
    if not prompt:
        raise JobError("prompt must not be empty")
    if len(prompt) > MAX_PROMPT_CHARS:
        raise JobError(f"prompt exceeds {MAX_PROMPT_CHARS} characters")

    frames = int(raw.get("frames", 81))
    fps = int(raw.get("fps", 24))
    steps = int(raw.get("steps", 30))
    width = int(raw.get("width", 1280))
    height = int(raw.get("height", 704))

    if not 1 <= frames <= MAX_FRAMES:
        raise JobError(f"frames must be 1..{MAX_FRAMES}")
    if not 1 <= fps <= 60:
        raise JobError("fps must be 1..60")
    if not 1 <= steps <= MAX_STEPS:
        raise JobError(f"steps must be 1..{MAX_STEPS}")
    if width % 16 or height % 16:
        # The VAE downsamples by 16; an odd size silently reshapes the output.
        raise JobError("width and height must be multiples of 16")
    if width * height > MAX_PIXELS:
        raise JobError(f"width*height must not exceed {MAX_PIXELS}")

    image = None
    if workload == "video.img2vid":
        name = raw.get("image")
        if not name:
            raise JobError("video.img2vid needs an `image`")
        if "/" in name or name.startswith("."):
            # The job dir is the only place a job may read from.
            raise JobError("`image` must be a bare filename in the job dir")
        image = job_dir / name
        if not image.is_file():
            raise JobError(f"conditioning image {name} not found in the job dir")

    seed = raw.get("seed")
    seed = random.randrange(2**31) if seed is None else int(seed)
    if not 0 <= seed < 2**32:
        raise JobError("seed must fit in 32 bits")

    return {
        "job_id": str(raw.get("job_id", "")),
        "workload": workload,
        "model": model,
        "prompt": prompt,
        "negative_prompt": (raw.get("negative_prompt") or "").strip(),
        "image": image,
        "frames": frames,
        "fps": fps,
        "steps": steps,
        "width": width,
        "height": height,
        "seed": seed,
        "guidance_scale": float(raw.get("guidance_scale", 5.0)),
    }


def generate_frames(job: dict, frame_dir: Path) -> None:
    """Run Wan 2.2 and write frames as zero-padded PNGs.

    Imported lazily: the selftest and the job-validation path must not need
    torch, which is most of the image.
    """
    import torch
    from diffusers import AutoencoderKLWan, WanImageToVideoPipeline, WanPipeline

    model: Model = job["model"]
    repo = model.diffusers_repo
    dtype = torch.float16 if model.precision == "fp16" else torch.bfloat16

    # The VAE stays in fp32: Wan's VAE overflows in half precision and the
    # symptom is a black clip rather than an error.
    vae = AutoencoderKLWan.from_pretrained(repo, subfolder="vae", torch_dtype=torch.float32)
    pipe_cls = WanPipeline if job["workload"] == "video.txt2vid" else WanImageToVideoPipeline
    pipe = pipe_cls.from_pretrained(repo, vae=vae, torch_dtype=dtype)
    pipe.to("cuda")

    kwargs = dict(
        prompt=job["prompt"],
        negative_prompt=job["negative_prompt"] or None,
        height=job["height"],
        width=job["width"],
        num_frames=job["frames"],
        num_inference_steps=job["steps"],
        guidance_scale=job["guidance_scale"],
        generator=torch.Generator(device="cuda").manual_seed(job["seed"]),
        # Not the default. diffusers hands back numpy frames unless asked for
        # PIL, and `write_frames` saves PIL images.
        output_type="pil",
    )
    if job["image"] is not None:
        from diffusers.utils import load_image

        kwargs["image"] = load_image(str(job["image"]))

    frames = pipe(**kwargs).frames[0]
    write_frames(frames, frame_dir)


def write_frames(frames, frame_dir: Path) -> int:
    """Save PIL frames as PNGs. Returns how many were written."""
    frame_dir.mkdir(parents=True, exist_ok=True)
    count = 0
    for i, frame in enumerate(frames):
        frame.save(frame_dir / f"frame_{i:05d}.png")
        count = i + 1
    if count == 0:
        raise JobError("the model produced no frames")
    return count


def encode(frame_dir: Path, out_path: Path, fps: int) -> None:
    """PNG sequence -> H.264 MP4.

    yuv420p and faststart because the buyer's browser is the common consumer
    and neither is the default for a PNG source.
    """
    cmd = [
        ffmpeg_bin(),
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-framerate",
        str(fps),
        "-i",
        str(frame_dir / "frame_%05d.png"),
        "-c:v",
        "libx264",
        "-preset",
        "medium",
        "-crf",
        "18",
        "-pix_fmt",
        "yuv420p",
        "-movflags",
        "+faststart",
        str(out_path),
    ]
    run(cmd, "ffmpeg encode")


def probe(out_path: Path) -> dict:
    """ffprobe the result. This is the plugin's declared validation, so a clip
    that does not probe as a playable video never becomes a receipt."""
    cmd = [
        ffprobe_bin(),
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height,nb_read_packets,r_frame_rate,codec_name",
        "-show_entries",
        "format=duration,size",
        "-count_packets",
        "-of",
        "json",
        str(out_path),
    ]
    out = run(cmd, "ffprobe")
    data = json.loads(out)
    streams = data.get("streams") or []
    if not streams:
        raise JobError("output has no video stream")
    stream = streams[0]
    fmt = data.get("format") or {}
    return {
        "codec": stream.get("codec_name"),
        "width": int(stream.get("width", 0)),
        "height": int(stream.get("height", 0)),
        "frames": int(stream.get("nb_read_packets", 0) or 0),
        "frame_rate": stream.get("r_frame_rate"),
        "duration_s": float(fmt.get("duration", 0) or 0),
        "size_bytes": int(fmt.get("size", 0) or 0),
    }


def validate(probed: dict, job: dict) -> None:
    if probed["width"] != job["width"] or probed["height"] != job["height"]:
        raise JobError(
            f"output is {probed['width']}x{probed['height']}, asked for "
            f"{job['width']}x{job['height']}"
        )
    if probed["frames"] != job["frames"]:
        raise JobError(f"output has {probed['frames']} frames, asked for {job['frames']}")
    if probed["duration_s"] <= 0:
        raise JobError("output has no duration")
    if probed["size_bytes"] > MAX_OUTPUT_BYTES:
        raise JobError(f"output exceeds the {MAX_OUTPUT_BYTES} byte result limit")


def run(cmd: list[str], what: str) -> str:
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        raise JobError(f"{what} failed: {proc.stderr.strip() or proc.returncode}")
    return proc.stdout


def ffmpeg_bin() -> str:
    return shutil.which("ffmpeg") or "ffmpeg"


def ffprobe_bin() -> str:
    return shutil.which("ffprobe") or "ffprobe"


def run_job(job_path: Path) -> int:
    job_dir = job_path.parent
    models = load_models(MODULE_TOML)
    job = parse_job(json.loads(job_path.read_text()), models, job_dir)

    frame_dir = job_dir / "frames"
    out_path = job_dir / "out.mp4"
    generate_frames(job, frame_dir)
    encode(frame_dir, out_path, job["fps"])
    probed = probe(out_path)
    validate(probed, job)
    write_sidecar(job_dir, job, probed)
    # The frames are many times the size of the clip and the node only ships
    # out.mp4, so they are dropped before the job dir is handed back.
    shutil.rmtree(frame_dir, ignore_errors=True)
    print(f"ok {out_path} {probed['frames']}f {probed['duration_s']:.2f}s")
    return 0


def write_sidecar(job_dir: Path, job: dict, probed: dict) -> None:
    model: Model = job["model"]
    sidecar = {
        "runner_version": RUNNER_VERSION,
        "job_id": job["job_id"],
        "workload": job["workload"],
        "model": model.id,
        "repo": model.repo,
        "diffusers_repo": model.diffusers_repo,
        "precision": model.precision,
        "prompt": job["prompt"],
        "negative_prompt": job["negative_prompt"],
        "seed": job["seed"],
        "steps": job["steps"],
        "guidance_scale": job["guidance_scale"],
        "requested": {
            "frames": job["frames"],
            "fps": job["fps"],
            "width": job["width"],
            "height": job["height"],
        },
        "produced": probed,
    }
    (job_dir / "out.json").write_text(json.dumps(sidecar, indent=2) + "\n")


def selftest(job_dir: Path) -> int:
    """Exercise the whole path with synthetic frames.

    Proves the parts that do not need a GPU: manifest parsing, job validation,
    PNG -> MP4 encode, ffprobe validation and the sidecar. If this fails on a
    provider's box, the job would have failed too.
    """
    job_dir.mkdir(parents=True, exist_ok=True)
    frames, fps, width, height = 8, 8, 64, 64
    job_path = job_dir / "job.json"
    job_path.write_text(
        json.dumps(
            {
                "job_id": "00000000-0000-0000-0000-000000000000",
                "workload": "video.txt2vid",
                "model": "wan2.2-ti2v-5b",
                "prompt": "selftest",
                "frames": frames,
                "fps": fps,
                "width": width,
                "height": height,
                "steps": 1,
                "seed": 7,
            }
        )
    )
    models = load_models(MODULE_TOML)
    job = parse_job(json.loads(job_path.read_text()), models, job_dir)

    frame_dir = job_dir / "frames"
    try:
        from PIL import Image

        synthetic = [
            Image.new("RGB", (width, height), (i * 8 % 256, 64, 128)) for i in range(frames)
        ]
        written = write_frames(synthetic, frame_dir)
    except ImportError:
        # No pillow: let ffmpeg make the frames so the encode path is still
        # covered on a box with nothing but ffmpeg.
        frame_dir.mkdir(parents=True, exist_ok=True)
        run(
            [
                ffmpeg_bin(),
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                f"testsrc=size={width}x{height}:rate={fps}",
                "-frames:v",
                str(frames),
                str(frame_dir / "frame_%05d.png"),
            ],
            "ffmpeg selftest frames",
        )
        written = frames
    if written != frames:
        raise JobError(f"wrote {written} frames, wanted {frames}")

    out_path = job_dir / "out.mp4"
    encode(frame_dir, out_path, fps)
    probed = probe(out_path)
    validate(probed, job)
    write_sidecar(job_dir, job, probed)
    shutil.rmtree(frame_dir, ignore_errors=True)
    print(
        f"selftest ok: {probed['codec']} {probed['width']}x{probed['height']} "
        f"{probed['frames']}f {probed['duration_s']:.2f}s {probed['size_bytes']}B"
    )
    print(f"models from manifest: {sorted(models)}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="c0mpute video-generation runner")
    parser.add_argument("command", choices=["txt2vid", "img2vid", "selftest"])
    parser.add_argument(
        "--job",
        default="/job/job.json",
        help="path to the job JSON (its directory is the job dir)",
    )
    args = parser.parse_args(argv)

    try:
        if args.command == "selftest":
            return selftest(Path(args.job).parent)
        return run_job(Path(args.job))
    except JobError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    except Exception as exc:  # noqa: BLE001 - the node only sees the exit code
        print(f"error: unexpected failure: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
