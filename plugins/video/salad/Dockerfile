# c0mpute GPU video worker for rented GPU containers (SaladCloud and the like).
#
# The runner image plus the released c0mpute binary, running the worker with
# only the video role in the in-container runner mode (C0MPUTE_VIDEO_RUNNER=
# local): there is no docker inside a rented container, so the node execs
# runner.py itself. See README.md beside this file.
#
# Built by .github/workflows/video-salad.yml:
#   docker build --build-arg C0MPUTE_VERSION=v0.3.37 \
#     -f plugins/video/salad/Dockerfile plugins/video
ARG RUNNER_IMAGE=ghcr.io/profullstack/c0mpute-video-runner:stable
FROM ${RUNNER_IMAGE}

USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl procps \
    && rm -rf /var/lib/apt/lists/*

# The release binary (static musl), checked against the release's sha256.
ARG C0MPUTE_VERSION
RUN set -eu; \
    test -n "${C0MPUTE_VERSION}"; \
    base="https://github.com/profullstack/c0mpute/releases/download/${C0MPUTE_VERSION}"; \
    cd /tmp; \
    curl -fsSLO "${base}/c0mpute-linux-x86_64.tar.gz"; \
    curl -fsSLO "${base}/c0mpute-linux-x86_64.tar.gz.sha256"; \
    sha256sum -c c0mpute-linux-x86_64.tar.gz.sha256; \
    tar -xzf c0mpute-linux-x86_64.tar.gz -C /usr/local/bin c0mpute; \
    rm -f c0mpute-linux-x86_64.tar.gz*; \
    c0mpute --version

COPY --chmod=0755 salad/entrypoint.sh salad/prefetch.py /opt/c0mpute-salad/

# Back to the runner's non-root user; it owns its home, the logs and the
# weights cache.
RUN mkdir -p /home/runner /logs /weights/hf \
    && chown -R 10001:10001 /home/runner /logs /weights
USER 10001:10001

# Local runner mode; weights cache on the container disk (warmed at start by
# prefetch.py, shared with the runner through HF_HOME). No self-update: the
# image pins the binary it was built with.
ENV C0MPUTE_VIDEO_RUNNER=local \
    C0MPUTE_VIDEO_RUNNER_PY=/opt/c0mpute-video/runner.py \
    C0MPUTE_NO_AUTO_UPDATE=1 \
    HF_HOME=/weights/hf \
    C0MPUTE_PREFETCH_MODELS=wan2.1-t2v-1.3b \
    C0MPUTE_NAME=profullstack \
    HOME=/home/runner

# Status page (boot + worker log) for the container gateway.
EXPOSE 8080
WORKDIR /home/runner
ENTRYPOINT ["/opt/c0mpute-salad/entrypoint.sh"]
