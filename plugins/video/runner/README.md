# video runner — Wan 2.2

The container the `video` plugin dispatches to. One job per process: read
`job.json`, generate a clip, encode an MP4, write a sidecar, exit non-zero on
any failure.

Published as `ghcr.io/profullstack/c0mpute-video-runner`. The tag a node pulls
is the one in [`../module.toml`](../module.toml) under `[module.dispatch]`.

## The contract

The node bind-mounts a job directory at `/job` and a weights cache at
`/weights`, then runs the image. Nothing is passed on stdout — the node reads
files and the process exit code.

| path | direction | what |
| --- | --- | --- |
| `/job/job.json` | in | the job |
| `/job/<image>` | in | `img2vid` only: the conditioning frame, a bare filename |
| `/job/out.mp4` | out | the clip, H.264 in MP4 |
| `/job/out.json` | out | sidecar: model, repo, seed, and what ffprobe saw |

```json
{
  "job_id": "uuid",
  "workload": "video.txt2vid",
  "model": "wan2.2-ti2v-5b",
  "prompt": "a neon alley in the rain",
  "negative_prompt": "",
  "frames": 81,
  "fps": 24,
  "steps": 30,
  "width": 1280,
  "height": 704,
  "seed": 12345
}
```

Exit codes: `0` done, `2` the job was refused (bad spec — do not retry), `1`
anything else.

## Models come from the manifest

`runner.py` reads `[module.models]` out of `module.toml`, which ships in the
image. A model the runner accepts and a model the marketplace advertises
therefore cannot drift apart, and adding one is a manifest row plus a rebuild.
Each row needs a `diffusers_repo`; rows without one are ignored by this
backend rather than breaking it.

## Why diffusers and not ComfyUI

ComfyUI would mean shipping hand-authored API graph JSON per model, which
cannot be checked by reading it and breaks silently when a node name changes
upstream. The diffusers pipelines take the Wan repos directly, so the model
table is the whole configuration.

## Build and run

```sh
# Context is plugins/video, because module.toml must be in the image.
docker build -t c0mpute/video-runner:dev -f plugins/video/runner/Dockerfile plugins/video

# Contract tests (no GPU, no weights, no docker)
cd plugins/video/runner && python3 test_runner.py

# Selftest: encode + probe + sidecar with synthetic frames (no GPU)
docker run --rm c0mpute/video-runner:dev selftest

# A real clip
mkdir -p job && cat > job/job.json <<'JSON'
{"job_id":"local","workload":"video.txt2vid","model":"wan2.2-ti2v-5b",
 "prompt":"a neon alley in the rain","frames":81,"fps":24,"steps":30,
 "width":1280,"height":704,"seed":7}
JSON
docker run --rm --gpus all \
  -v "$PWD/weights:/weights" -v "$PWD/job:/job" \
  c0mpute/video-runner:dev txt2vid
```

The build runs `test_runner.py` and imports torch and diffusers, so a bad
dependency pin or a broken contract fails the build rather than reaching a
provider mid-auction.

## Weights, and why the cache matters

First use of a model pulls tens of GB from Hugging Face into
`/weights/hf`. On a rented card that pull bills at the same rate as
inference, so warm the cache before advertising the role. Moving weights to
the c0mpute storage plugin (DIP-0012) is the next step and would also let the
container run with `--network none`.

## What is verified, and what is not

Verified in CI and locally: manifest parsing, job validation and refusals,
PNG→MP4 encode, ffprobe validation, the sidecar, and that this pin set
imports.

**Not yet verified: a real clip.** That needs a 24 GiB NVIDIA card, which no
node in the fleet currently has. The generation path (`generate_frames`) is
written against the diffusers 0.42 `WanPipeline` / `WanImageToVideoPipeline`
signatures but has never produced a frame. Treat the first real job as the
test it is.
