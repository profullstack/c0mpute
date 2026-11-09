# video runner — Wan 2.2 / 2.1 and LTX-2.3 (video + audio)

The container the `video` plugin dispatches to. One job per process: read
`job.json`, generate a clip, encode an MP4, write a sidecar, exit non-zero on
any failure.

Published as `ghcr.io/profullstack/c0mpute-video-runner` with the tags
`:stable`, `:candidate` and `:<commit sha>` on every master build. A node
pulls by digest: the one in [`../module.toml`](../module.toml) under
`[module.dispatch]` (and `RUNNER_DIGEST` in `video.rs`), then tags it locally
as `c0mpute/video-runner:stable`, the name the worker runs. After a runner
change, copy the digest from the workflow's job summary into both pins.

**The registry is a convenience, not a dependency.** `c0mpute video setup`
pulls the image, and if the registry will not serve it — private package,
rate limit, outage — it shallow-clones this public repo and builds the same
image locally. `c0mpute video setup --build` skips the pull entirely. A node
with a card can therefore always serve video, with no registry account.

## The contract

The node bind-mounts a job directory at `/job` and a weights cache at
`/weights`, then runs the image. Nothing is passed on stdout — the node reads
files and the process exit code.

| path | direction | what |
| --- | --- | --- |
| `/job/job.json` | in | the job |
| `/job/<image>` | in | `img2vid` only: the conditioning frame, a bare filename |
| `/job/out.mp4` | out | the clip, H.264 in MP4 (plus AAC audio for `audio = true` models) |
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

A row may also set its own job defaults (`frames`, `fps`, `steps`, `width`,
`height`) for fields a job leaves out; the node's gateway applies the same
ones. Every Wan model needs `frames` = 4k+1 and the runner refuses any other
count up front (the pipeline would round it and fail validation afterwards).

## Small cards: Wan 2.1 T2V 1.3B on 8 GiB

`wan2.1-t2v-1.3b` (`Wan-AI/Wan2.1-T2V-1.3B-Diffusers`, Apache-2.0) is the
model an 8 GiB card can serve. Its row says `vram_gib = 8` (the floor with
offload) and `full_vram_gib = 20` (everything on the card at once). When the
card is smaller than `full_vram_gib` the runner:

- encodes the prompt on the CPU first: the UMT5-XXL text encoder is ~11 GB in
  bf16, and model offload would still move it onto the card whole, so the
  pipeline gets embeddings and never calls the encoder;
- then `enable_model_cpu_offload()` (transformer and VAE take turns on the
  card) and VAE tiling for the decode.

Expect ~16 GB of system RAM for the encoder on the host. Defaults are small:
33 frames of 480x272 at 16 fps, 20 steps. The sidecar records `offload`.
CPU-only nodes serve the same model id without this runner, through
stable-diffusion.cpp (`node/crates/c0mpute-core/src/video_cpu.rs`).

## Why diffusers and not ComfyUI

ComfyUI would mean shipping hand-authored API graph JSON per model, which
cannot be checked by reading it and breaks silently when a node name changes
upstream. The diffusers pipelines take the Wan repos directly, so the model
table is the whole configuration.

## Video with sound: LTX-2.3

`ltx2.3-av-distilled` (`backend = "ltx2"`, `audio = true`) runs
`diffusers/LTX-2.3-Distilled-Diffusers` through `LTX2Pipeline` /
`LTX2ImageToVideoPipeline`. One call returns the frames and a 48 kHz stereo
waveform; the runner writes the waveform as a WAV and ffmpeg muxes it as AAC.
(diffusers' own `encode_video` would need PyAV; ffmpeg is already here.)

- `frames` must be 8k+1 (81, 121, 161...) and width/height divisible by 32.
  The node's defaults (81 frames, 1280x704) already are.
- The distilled checkpoint runs its fixed 8-step sigma schedule whatever
  `steps` says, with CFG off (`guidance_scale` 1.0) unless the job sets one.
  The sidecar records the 8.
- A clip from an audio model with no audio stream fails validation: a buyer
  who paid for sound never gets a silent clip.
- 22B parameters in bf16 with model CPU offload: the 48 GiB floor in the
  manifest is the transformer plus headroom. A 24 GiB card would need
  sequential offload and 64 GB+ of system RAM, which is too slow to win jobs.

**License.** The weights are under the LTX-2 Community License, not Apache:
free for entities under $10M annual revenue, hosting as a service is allowed
(s.3), Attachment A's use restrictions must be in the buyer terms, outputs
must be disclosed as machine generated, and watermark/provenance features
must not be stripped. Its item 20 forbids use in a service that "directly
competes" with Lightricks' commercial offerings without a separate license;
we have asked Lightricks to confirm a compute marketplace is not that.

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

First use of a model pulls from Hugging Face into `/weights/hf`. Measured
for the default `wan2.2-ti2v-5b`: **32 GB** over 242 shards. On a rented card
that pull bills at the same rate as inference, so warm the cache before
advertising the role. Moving weights to
the c0mpute storage plugin (DIP-0012) is the next step and would also let the
container run with `--network none`.

## What is verified, and what is not

Verified in CI and locally: manifest parsing, job validation and refusals,
PNG→MP4 encode, ffprobe validation, the sidecar, and that this pin set
imports.

**Not yet verified: a real clip on a GPU.** The fleet's one NVIDIA card has
8 GiB, which is what `wan2.1-t2v-1.3b` and its offload path are for; the
24/48 GiB models have no card yet. The same model on CPU (sd.cpp, not this
runner) has produced real clips. The LTX-2 sampling
call is written against the diffusers 0.42.1 source and the model card, and
its audio mux is covered by the selftest with a synthetic tone, but the
pipeline itself has never run here.

How far the generation path has actually been taken, on a box with no GPU:
`txt2vid` resolved the diffusers repo, pulled the weights and loaded 62% of
the model's 242 shards before the kernel killed it for memory. So the repo
id, the pipeline class and `from_pretrained` are real; everything from
`pipe.to("cuda")` onward — the sampling call, its kwargs, and the PIL frames
`write_frames` expects — has never executed. Treat the first real job as the
test it is.
