# syntax=docker/dockerfile:1.7
#
# c0mpute's CrowdGPT runner: CrowdGPT's own client, built from its public
# repository at one reviewed commit, run headless under c0mpute's idle
# governor. This Dockerfile and c0mpute-crowdgpt-run are c0mpute's (MIT).
# The client it fetches is CrowdGPT's (GPL-3.0); it is not modified, and its
# source is exactly github.com/Vxtzq/CrowdGPT at CROWDGPT_COMMIT (also in the
# image labels and at /app). c0mpute never links or imports it: the node only
# runs this image and reads its log lines.
#
# The default build is CPU (python slim + the CPU torch wheel). A CUDA build:
#   docker build --build-arg BASE_IMAGE=pytorch/pytorch:2.6.0-cuda12.4-cudnn9-runtime \
#                --build-arg TORCH_INDEX= -t c0mpute/crowdgpt-runner:stable .
# (`c0mpute crowdgpt setup --build --backend cuda` does exactly that.)

ARG BASE_IMAGE=python:3.11-slim-bookworm
FROM ${BASE_IMAGE}

ARG CROWDGPT_REPO=https://github.com/Vxtzq/CrowdGPT.git
ARG CROWDGPT_COMMIT=b7d5b399c74c1c2e1d23008b56d2ce953a8189d8
ARG TORCH_INDEX=https://download.pytorch.org/whl/cpu
ARG TORCH_SPEC="torch>=2.6,<2.10"

LABEL org.c0mpute.crowdgpt.commit="${CROWDGPT_COMMIT}" \
      org.c0mpute.crowdgpt.repo="${CROWDGPT_REPO}" \
      org.opencontainers.image.description="CrowdGPT client (GPL-3.0, unmodified) run by c0mpute's idle-donation governor" \
      org.opencontainers.image.licenses="GPL-3.0-only AND MIT"

ENV PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1 \
    TOKENIZERS_PARALLELISM=false \
    CROWDGPT_DATA_DIR=/data \
    HF_HOME=/data/hf_home \
    TORCH_HOME=/data/torch_home \
    XDG_CACHE_HOME=/data/cache \
    HOME=/data/home

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends git ca-certificates \
 && if ! command -v python >/dev/null 2>&1; then ln -s "$(command -v python3)" /usr/local/bin/python; fi \
 && rm -rf /var/lib/apt/lists/*

# The client at exactly the pinned commit; refuse anything else.
RUN git clone --filter=blob:none "${CROWDGPT_REPO}" /app \
 && cd /app \
 && git checkout --detach "${CROWDGPT_COMMIT}" \
 && test "$(git rev-parse HEAD)" = "${CROWDGPT_COMMIT}" \
 && rm -rf .git

# Dependencies the way CrowdGPT's own docker/install-deps.sh does it: torch
# from TORCH_INDEX (the base image's own torch when it is new enough), then
# requirements.txt minus torch and pywebview (the GUI, unused headless).
RUN --mount=type=cache,target=/root/.cache/pip \
    set -eu; \
    if python -c 'import torch,sys; v=tuple(int(x) for x in torch.__version__.split("+")[0].split(".")[:2]); sys.exit(0 if v >= (2,6) else 1)' 2>/dev/null; then \
      echo "reusing base image torch"; \
    elif [ -n "${TORCH_INDEX}" ]; then \
      python -m pip install "${TORCH_SPEC}" --index-url "${TORCH_INDEX}"; \
    else \
      python -m pip install "${TORCH_SPEC}"; \
    fi; \
    grep -Ev '^\s*(torch|torchvision|torchaudio|torch-directml|pywebview)(\s*[<>=!~].*)?$' /app/requirements.txt > /tmp/req.txt; \
    python -m pip install -r /tmp/req.txt; \
    rm -f /tmp/req.txt; \
    python -c 'import torch, safetensors, huggingface_hub; print("torch", torch.__version__)'

# Read-only at run time: the app (owned by root, never writable by the
# client's uid) and a checkpoints link into the one writable mount, /data.
RUN rm -rf /app/checkpoints \
 && ln -sfn /data/checkpoints /app/checkpoints \
 && mkdir -p /data \
 && chmod 0777 /data

COPY c0mpute-crowdgpt-run /usr/local/bin/c0mpute-crowdgpt-run
RUN chmod 0755 /usr/local/bin/c0mpute-crowdgpt-run

WORKDIR /app
# c0mpute runs it as the node user (`--user <uid>:<gid>` of the cache dir);
# never root, even when started by hand.
USER 65534:65534
ENTRYPOINT ["/usr/local/bin/c0mpute-crowdgpt-run"]
CMD []
