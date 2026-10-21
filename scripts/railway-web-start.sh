#!/usr/bin/env sh
# Railway entrypoint for the c0mpute.com service (DIP-0014).
#
# One deploy, two co-located processes:
#   - the status aggregator (Rust libp2p observer) in the background, and
#   - the Next.js site in the foreground.
# The site proxies /status + /api/status to the aggregator over loopback
# (STATUS_AGGREGATOR_URL=http://127.0.0.1:8090). If the aggregator ever exits,
# the site keeps serving — /status just falls back to the stub payload.
set -eu

# Background: status aggregator, restarted whenever it exits (backoff 2 s
# doubling to 60 s). Its exit must never kill the container; the site
# (foreground) is what the host tracks for liveness. The site also reads the
# other aggregators in STATUS_AGGREGATOR_URLS, so a restart here is invisible.
(
  delay=2
  while :; do
    started=$(date +%s)
    c0mpute status-aggregator || true
    echo "status-aggregator exited; restarting in ${delay}s" >&2
    sleep "$delay"
    # A run that lasted 10 minutes was healthy: start the backoff over.
    if [ $(( $(date +%s) - started )) -ge 600 ]; then delay=2; else delay=$(( delay * 2 > 60 ? 60 : delay * 2 )); fi
  done
) &

# Foreground: the Next.js server. exec so it becomes the container's main
# process and receives Railway's signals directly.
exec bun run --filter=c0mpute start
