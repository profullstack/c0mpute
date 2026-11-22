# c0mpute worker image: ghcr.io/profullstack/c0mpute-worker
#
# Runs a c0mpute node (https://c0mpute.com) in a container: the signed c0mpute
# release plus the coinpay and infernet CLIs, installed by the same
# https://c0mpute.com/install.sh every other node uses, as a non-root user.
# Everything the node writes (identity keys, config, jobs, models, shards)
# lives under /data, so mount a volume there or the node gets a new identity
# on every restart. See docker/worker/README.md for the env vars.
#
# Build context is this directory:
#   docker build -t c0mpute-worker docker/worker
#   docker build --build-arg C0MPUTE_VERSION=0.3.99 -t c0mpute-worker docker/worker   (v0.3.99 works too)
FROM debian:bookworm-slim

ARG C0MPUTE_VERSION=latest

# ffmpeg: transcode role. python3-venv: tts/stt roles build their venvs at
# first start. git, unzip, xz and zstd: the coinpay and infernet installers. tini: PID 1.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl git unzip xz-utils zstd procps tini \
      python3 python3-venv ffmpeg \
 && rm -rf /var/lib/apt/lists/*

RUN groupadd --gid 1000 c0mpute \
 && useradd --uid 1000 --gid 1000 --home-dir /data --no-create-home --shell /bin/sh c0mpute \
 && mkdir -p /opt/c0mpute /data \
 && chown -R c0mpute:c0mpute /opt/c0mpute /data

USER c0mpute

# Install into /opt/c0mpute (part of the image); the runtime HOME is /data.
# No autostart, no model pulls and no relocation at build time: models and
# runtimes are fetched by the worker into /data on first start.
RUN export HOME=/opt/c0mpute \
      C0MPUTE_HOME=/opt/c0mpute/.c0mpute \
      C0MPUTE_VERSION="$(case "${C0MPUTE_VERSION}" in latest|v*) echo "${C0MPUTE_VERSION}" ;; *) echo "v${C0MPUTE_VERSION}" ;; esac)" \
      NO_STORAGE_RELOCATE=1 \
      INFERNET_NO_RELOCATE=1 \
      INFERNET_AUTOSTART=0 \
      INFERNET_SKIP_CPU_BASELINE=1 \
      INFERNET_SKIP_DISK_CHECK=1 \
      INFERNET_NO_AUTOUPDATE=1 \
      INFERNET_INSTALL_VLLM=0 \
 && curl -fsSL https://c0mpute.com/install.sh -o /tmp/install.sh \
 && sh /tmp/install.sh --no-start --no-exec --no-tui \
 && rm -f /tmp/install.sh \
 && rm -rf /opt/c0mpute/.bun/install/cache /opt/c0mpute/.cache /opt/c0mpute/.npm \
 && /opt/c0mpute/.c0mpute/bin/c0mpute --version \
 && test -x /opt/c0mpute/.local/bin/infernet \
 && test -x /opt/c0mpute/.local/bin/coinpay

COPY --chown=c0mpute:c0mpute entrypoint.sh status_ui.py /opt/c0mpute/

ENV HOME=/data \
    C0MPUTE_HOME=/data/.c0mpute \
    PATH=/opt/c0mpute/.c0mpute/bin:/opt/c0mpute/.local/bin:/opt/c0mpute/.bun/bin:/opt/c0mpute/.coinpay/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    MISE_DATA_DIR=/opt/c0mpute/.local/share/mise \
    MISE_GLOBAL_CONFIG_FILE=/opt/c0mpute/.config/mise/config.toml \
    MISE_TRUSTED_CONFIG_PATHS=/opt/c0mpute/.config/mise \
    MISE_YES=1 \
    C0MPUTE_NO_AUTO_UPDATE=1 \
    INFERNET_NO_AUTOUPDATE=1 \
    NO_STORAGE_RELOCATE=1 \
    INFERNET_NO_RELOCATE=1 \
    C0MPUTE_P2P_PORT=46337 \
    C0MPUTE_DISABLE_MDNS=1 \
    C0MPUTE_UI_BIND=0.0.0.0:8080

VOLUME ["/data"]
WORKDIR /data
# 8080/tcp: status page (status_ui.py); no login, keep it on your LAN.
# 46337/tcp: libp2p (optional to publish; outbound alone is enough to join).
# 7777/tcp: HTTP gateway, only when the gateway role is on.
EXPOSE 8080/tcp 46337/tcp 7777/tcp

HEALTHCHECK --interval=60s --timeout=10s --start-period=120s --retries=3 \
  CMD pgrep -f "c0mpute worker start" >/dev/null || exit 1

ENTRYPOINT ["/usr/bin/tini", "--", "/opt/c0mpute/entrypoint.sh"]
CMD ["worker", "start"]
