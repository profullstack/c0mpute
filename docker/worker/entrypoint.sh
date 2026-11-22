#!/bin/sh
# Entrypoint for ghcr.io/profullstack/c0mpute-worker.
#
# Default command: `c0mpute worker start` in the foreground, plus the status
# page on :8080. Other arguments run as a c0mpute subcommand
# (`docker run ... status`), and a first argument that is an executable on
# PATH runs as is (`docker run ... infernet payout list`).
set -eu

mkdir -p "$HOME" "$C0MPUTE_HOME"

if [ "${1:-}" = "worker" ] && [ "${2:-}" = "start" ]; then
  # Extra roles / flags from the environment:
  #   C0MPUTE_ROLES=tts,stt,transcode   C0MPUTE_WORKER_ARGS="--no-hosting"
  [ -n "${C0MPUTE_ROLES:-}" ] && set -- "$@" --roles "$C0MPUTE_ROLES"
  # shellcheck disable=SC2086
  [ -n "${C0MPUTE_WORKER_ARGS:-}" ] && set -- "$@" $C0MPUTE_WORKER_ARGS

  # Status page (C0MPUTE_UI=0 turns it off).
  if [ "${C0MPUTE_UI:-1}" != "0" ]; then
    python3 /opt/c0mpute/status_ui.py &
  fi

  # Payout address for inference earnings: `infernet payout set` needs a
  # registered node, which the worker does on its first start, so retry in the
  # background for a while instead of failing the container.
  if [ -n "${C0MPUTE_PAYOUT_ADDRESS:-}" ] && [ -n "${C0MPUTE_PAYOUT_COIN:-}" ]; then
    (
      i=0
      while [ "$i" -lt 60 ]; do
        sleep 10
        if infernet payout set "$C0MPUTE_PAYOUT_COIN" "$C0MPUTE_PAYOUT_ADDRESS" \
             ${C0MPUTE_PAYOUT_NETWORK:+--network "$C0MPUTE_PAYOUT_NETWORK"} >/dev/null 2>&1; then
          echo "c0mpute-worker: payout address set ($C0MPUTE_PAYOUT_COIN)"
          exit 0
        fi
        i=$((i + 1))
      done
      echo "c0mpute-worker: could not set the payout address after 10 minutes;" \
           "set it on the status page or run: docker exec <container> infernet payout set <coin> <address>" >&2
    ) &
  fi
  exec c0mpute "$@"
fi

if [ "$#" -gt 0 ] && command -v "$1" >/dev/null 2>&1; then
  exec "$@"
fi
exec c0mpute "$@"
