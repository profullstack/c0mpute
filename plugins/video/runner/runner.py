#!/usr/bin/env python3
"""c0mpute video-generation runner.

One job per process: read a job JSON, generate a clip (Wan 2.2, or LTX-2.3
with a synchronized soundtrack), encode an MP4, write a metadata sidecar, and
exit non-zero on any failure. The node's
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

RUNNER_VERSION = "2"

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

# Pipeline families. A manifest row without `backend` is Wan, which is what
# every row meant before LTX-2 arrived.
BACKENDS = ("wan", "ltx2")

# The distilled LTX-2 checkpoints only work on their fixed sigma schedule;
# `steps` from the job is ignored for them and this is what ran.
LTX2_DISTILLED_STEPS = 8


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
    backend: str = "wan"
    audio: bool = False
    distilled: bool = False


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
            backend=row.get("backend", "wan"),
            audio=bool(row.get("audio", False)),
            distilled=bool(row.get("distilled", False)),
        )
        if models[mid].backend not in BACKENDS:
            # Fail loudly: a typo here would otherwise advertise a model the
            # runner then serves with the wrong pipeline.
            raise JobError(f"model {mid} has unknown backend {models[mid].backend!r}")
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
    if model.backend == "ltx2":
        # The pipeline raises on sizes not divisible by 32, and silently
        # rounds a frame count that is not 8k+1 (its VAE compresses time by
        # 8), which would then fail the exact frame-count validation after
        # the worker had already paid for the generation.
        if width % 32 or height % 32:
            raise JobError(f"{model.id}: width and height must be multiples of 32")
        if frames % 8 != 1:
            raise JobError(f"{model.id}: frames must be 8k+1 (e.g. 81, 121, 161)")
        if model.distilled:
            steps = LTX2_DISTILLED_STEPS

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
        # Distilled checkpoints are trained for CFG off; 5.0 on them burns
        # two forward passes per step for a worse clip.
        "guidance_scale": float(
            raw["guidance_scale"]
            if raw.get("guidance_scale") is not None
            else (1.0 if model.distilled else 5.0)
        ),
    }


def generate(job: dict, frame_dir: Path, audio_path: Path) -> bool:
    """Run the model's pipeline. Writes frames as PNGs, and the soundtrack as
    a WAV when the model makes one. Returns whether audio was written."""
    if job["model"].backend == "ltx2":
        return generate_ltx2(job, frame_dir, audio_path)
    generate_frames(job, frame_dir)
    return False


def generate_ltx2(job: dict, frame_dir: Path, audio_path: Path) -> bool:
    """LTX-2.x: video and a synchronized soundtrack from one pipeline call.

    diffusers' own `encode_video` muxes the audio but needs PyAV; we already
    ship ffmpeg for the encode and ffprobe for validation, so the waveform is
    written as a WAV and muxed by `encode` instead of adding a dependency.
    """
    import torch
    from diffusers import LTX2ImageToVideoPipeline, LTX2Pipeline
    from diffusers.pipelines.ltx2.utils import DEFAULT_NEGATIVE_PROMPT, DISTILLED_SIGMA_VALUES

    model: Model = job["model"]
    pipe_cls = LTX2Pipeline if job["workload"] == "video.txt2vid" else LTX2ImageToVideoPipeline
    pipe = pipe_cls.from_pretrained(model.diffusers_repo, torch_dtype=torch.bfloat16)
    # Model offload, not .to("cuda"): the 22B transformer alone is most of a
    # 48 GiB card, and the Gemma text encoder does not need to sit beside it.
    pipe.enable_model_cpu_offload()
    pipe.vae.enable_tiling()

    kwargs = dict(
        prompt=job["prompt"],
        negative_prompt=job["negative_prompt"] or DEFAULT_NEGATIVE_PROMPT,
        height=job["height"],
        width=job["width"],
        num_frames=job["frames"],
        frame_rate=float(job["fps"]),
        num_inference_steps=job["steps"],
        guidance_scale=job["guidance_scale"],
        generator=torch.Generator(device="cpu").manual_seed(job["seed"]),
        output_type="pil",
        return_dict=False,
    )
    if model.distilled:
        kwargs["sigmas"] = DISTILLED_SIGMA_VALUES
    if job["image"] is not None:
        from diffusers.utils import load_image

        kwargs["image"] = load_image(str(job["image"]))

    video, audio = pipe(**kwargs)
    write_frames(video[0], frame_dir)
    rate = int(pipe.vocoder.config.output_sampling_rate)
    write_wav(audio[0].float().cpu().numpy(), rate, audio_path)
    return True


def write_wav(samples, rate: int, path: Path) -> None:
    """Float waveform shaped [channels, samples] (or [samples]) in -1..1 ->
    16-bit PCM WAV. Pure stdlib so the selftest covers it without numpy."""
    import array
    import wave

    rows = samples.tolist() if hasattr(samples, "tolist") else samples
    if rows and not isinstance(rows[0], (list, tuple)):
        rows = [rows]
    channels = len(rows)
    if channels == 0 or channels > 8 or not rows[0]:
        raise JobError("the model produced no audio")
    length = len(rows[0])
    if any(len(r) != length for r in rows):
        raise JobError("audio channels differ in length")
    pcm = array.array("h", bytes(2 * channels * length))
    for c, row in enumerate(rows):
        for i, v in enumerate(row):
            v = -1.0 if v < -1.0 else 1.0 if v > 1.0 else v
            pcm[i * channels + c] = int(round(v * 32767))
    if sys.byteorder == "big":
        pcm.byteswap()
    with wave.open(str(path), "wb") as w:
        w.setnchannels(channels)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(pcm.tobytes())


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


def encode(frame_dir: Path, out_path: Path, fps: int, audio_path: Path | None = None) -> None:
    """PNG sequence (+ optional WAV) -> H.264/AAC MP4.

    yuv420p and faststart because the buyer's browser is the common consumer
    and neither is the default for a PNG source.
    """
    audio_in: list[str] = []
    audio_out: list[str] = []
    if audio_path is not None:
        # No -shortest: it can drop the last video frame and fail the exact
        # frame-count check. The model sizes its audio to the clip.
        audio_in = ["-i", str(audio_path)]
        audio_out = ["-c:a", "aac", "-b:a", "192k"]
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
        *audio_in,
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
        *audio_out,
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
    audio = json.loads(
        run(
            [
                ffprobe_bin(),
                "-v",
                "error",
                "-select_streams",
                "a:0",
                "-show_entries",
                "stream=codec_name,sample_rate,channels",
                "-of",
                "json",
                str(out_path),
            ],
            "ffprobe audio",
        )
    ).get("streams") or []
    return {
        "audio": (
            {
                "codec": audio[0].get("codec_name"),
                "sample_rate": int(audio[0].get("sample_rate", 0) or 0),
                "channels": int(audio[0].get("channels", 0) or 0),
            }
            if audio
            else None
        ),
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
    if job["model"].audio and not probed.get("audio"):
        # A model sold as "video + audio" must not hand back a silent clip.
        raise JobError(f"{job['model'].id} must produce a soundtrack; output has none")


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
    audio_path = job_dir / "audio.wav"
    has_audio = generate(job, frame_dir, audio_path)
    encode(frame_dir, out_path, job["fps"], audio_path if has_audio else None)
    probed = probe(out_path)
    validate(probed, job)
    write_sidecar(job_dir, job, probed)
    # The frames are many times the size of the clip and the node only ships
    # out.mp4, so they are dropped before the job dir is handed back.
    shutil.rmtree(frame_dir, ignore_errors=True)
    audio_path.unlink(missing_ok=True)
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
        "backend": model.backend,
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
    print(
        f"selftest ok: {probed['codec']} {probed['width']}x{probed['height']} "
        f"{probed['frames']}f {probed['duration_s']:.2f}s {probed['size_bytes']}B"
    )

    # The soundtrack path: WAV writer -> AAC mux -> ffprobe sees the audio
    # stream -> an audio model's validation passes. Same frames, re-used.
    audio_models = [m for m in models.values() if m.audio]
    if audio_models:
        import math

        model = audio_models[0]
        av_frames = 9  # 8k+1, as LTX-2 requires
        if written < av_frames:
            # Pad by repeating the last frame; the encode only needs files.
            last = frame_dir / f"frame_{written - 1:05d}.png"
            for i in range(written, av_frames):
                shutil.copyfile(last, frame_dir / f"frame_{i:05d}.png")
        av_job = parse_job(
            {**json.loads(job_path.read_text()), "model": model.id, "frames": av_frames},
            models,
            job_dir,
        )
        rate = 48000
        n = int(rate * av_frames / fps)
        tone = [math.sin(2 * math.pi * 440 * i / rate) * 0.2 for i in range(n)]
        audio_path = job_dir / "audio.wav"
        write_wav([tone, tone], rate, audio_path)
        encode(frame_dir, out_path, fps, audio_path)
        av_probed = probe(out_path)
        validate(av_probed, av_job)
        write_sidecar(job_dir, av_job, av_probed)
        audio_path.unlink(missing_ok=True)
        a = av_probed["audio"]
        print(
            f"selftest audio ok ({model.id}): {a['codec']} {a['sample_rate']}Hz "
            f"{a['channels']}ch, {av_probed['frames']}f"
        )

    shutil.rmtree(frame_dir, ignore_errors=True)
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
