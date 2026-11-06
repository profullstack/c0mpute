# c0mpute video-generation runner — Wan 2.2 via diffusers.
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

WORKDIR /opt/c0mpute-video

COPY runner/requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

# The manifest is the model registry the runner reads at run time, so it
# ships in the image rather than being duplicated in Python.
COPY module.toml ./
COPY runner/runner.py runner/test_runner.py ./

# Fail the BUILD if this combination of pins cannot even import, or if the
# runner's own contract tests do not pass. A published image that cannot
# import diffusers would otherwise be discovered by a provider, mid-auction.
RUN python3 -c "\
import torch, diffusers;\
from diffusers import AutoencoderKLWan, WanImageToVideoPipeline, WanPipeline;\
print('torch', torch.__version__, '| diffusers', diffusers.__version__)" \
    && C0MPUTE_VIDEO_MODULE_TOML=/opt/c0mpute-video/module.toml \
       python3 /opt/c0mpute-video/test_runner.py

# Weights cache. Mount a named volume here: a cold Hugging Face pull is tens
# of GB, and on a rented card that pull is billed like inference.
ENV HF_HOME=/weights/hf \
    HF_HUB_DISABLE_TELEMETRY=1 \
    PYTHONUNBUFFERED=1
VOLUME ["/weights"]

# The job directory the node bind-mounts. Commands: txt2vid | img2vid | selftest.
WORKDIR /job
ENTRYPOINT ["python3", "/opt/c0mpute-video/runner.py"]
CMD ["selftest"]
