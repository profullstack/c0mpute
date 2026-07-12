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

# Background: status aggregator. Its exit must never kill the container; the
# site (foreground) is what Railway tracks for liveness.
(
  c0mpute status-aggregator || echo "status-aggregator exited ($?)" >&2
) &

# Foreground: the Next.js server. exec so it becomes the container's main
# process and receives Railway's signals directly.
exec bun run --filter=c0mpute start
