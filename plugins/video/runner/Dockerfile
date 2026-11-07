# c0mpute video-generation runner — Wan 2.2 and LTX-2.3 (video + audio) via diffusers.
#
# Published as ghcr.io/profullstack/c0mpute-video-runner (see
# .github/workflows/video-runner.yml); the tag the node pulls is the one in
# plugins/video/module.toml under [module.dispatch].
#
# Build from plugins/video (the context must include module.toml):
#   docker build -t c0mpute/video-runner:dev -f plugins/video/runner/Dockerfile plugins/video
#
# Run by hand:
#   docker run --rm --gpus all \
#     -v c0mpute-video-weights:/weights \
#     -v "$PWD/job:/job" \
#     ghcr.io/profullstack/c0mpute-video-runner:candidate txt2vid
#
# No CUDA base image on purpose: the torch wheels from PyPI bring their own
# CUDA runtime, so only the driver comes from the host via the NVIDIA
# container runtime. See requirements.txt.
FROM python:3.12-slim

# ffmpeg is not optional — it encodes the clip and ffprobe is the plugin's
# declared validation, so an image without it can only fail jobs.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ffmpeg \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# No .pyc litter from the build-time test run, and none at run time in a
# read-only directory.
ENV PYTHONDONTWRITEBYTECODE=1

WORKDIR /opt/c0mpute-video

# --chmod, not inherited modes: COPY preserves the build context's
# permissions, so a checkout without "other" read bits (umask, ACLs)
# builds an image whose own non-root USER cannot read its runner. CI
# checks out 644 and would never show it.
COPY --chmod=0644 runner/requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

# The manifest is the model registry the runner reads at run time, so it
# ships in the image rather than being duplicated in Python.
COPY --chmod=0644 module.toml ./
COPY --chmod=0755 runner/runner.py runner/test_runner.py ./

# Fail the BUILD if this combination of pins cannot even import, or if the
# runner's own contract tests do not pass. A published image that cannot
# import diffusers would otherwise be discovered by a provider, mid-auction.
RUN python3 -c "\
import torch, diffusers;\
from diffusers import AutoencoderKLWan, WanImageToVideoPipeline, WanPipeline;\
from diffusers import LTX2Pipeline, LTX2ImageToVideoPipeline;\
from diffusers.pipelines.ltx2.utils import DEFAULT_NEGATIVE_PROMPT, DISTILLED_SIGMA_VALUES;\
print('torch', torch.__version__, '| diffusers', diffusers.__version__)" \
    && C0MPUTE_VIDEO_MODULE_TOML=/opt/c0mpute-video/module.toml \
       python3 /opt/c0mpute-video/test_runner.py

# Weights cache. Mount a named volume or a host directory here: a cold
# Hugging Face pull is tens of GB, and on a rented card that pull is billed
# like inference.
ENV HF_HOME=/weights/hf \
    HF_HUB_DISABLE_TELEMETRY=1 \
    PYTHONUNBUFFERED=1
VOLUME ["/weights"]

# Never root. The node passes `--user <its own uid>:<gid>` so the clip it
# reads back is owned by the node rather than by root — without that it
# cannot clean up its own job directory. This USER is for every other way
# the image gets run (by hand, or the CI selftest), where there is no
# --user to override it. Numeric so it resolves with no passwd lookup.
RUN useradd --uid 10001 --user-group --no-create-home --shell /usr/sbin/nologin runner \
    && mkdir -p /job /weights \
    && chown 10001:10001 /job /weights
USER 10001:10001

# The job directory the node bind-mounts. Commands: txt2vid | img2vid | selftest.
WORKDIR /job
ENTRYPOINT ["python3", "/opt/c0mpute-video/runner.py"]
CMD ["selftest"]
