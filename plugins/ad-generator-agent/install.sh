#!/usr/bin/env sh
# ad-generator-agent plugin installer.
#
# ad-generator-agent is a buyer-side agent: it runs inside the application that
# creates ads (crawlproof.com is the first) and buys existing c0mpute workloads.
# There is nothing to install on a worker. Workers earn from it by serving the
# workloads it buys.
#
# Served at https://c0mpute.com/plugins/ad-generator-agent/install.sh.
set -eu

ok()   { printf '\033[1;32m✓\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }

if command -v c0mpute >/dev/null 2>&1; then
  ok "c0mpute $(c0mpute --version 2>/dev/null || echo installed) found"
else
  warn "c0mpute is not installed: curl -fsSL https://c0mpute.com/install.sh | sh"
fi
cat <<'TXT'

  Use it:   https://crawlproof.com/dashboard/ads/new  (on by default)

  Earn from it (serve the workloads it buys):
    c0mpute tts setup --engine kokoro && c0mpute worker start --roles tts
    c0mpute plugin install infernet      # ad copy (Infernet inference)

  Run your own buyer gateway for narration:
    C0MPUTE_SPEECH_TOKEN=<random 32+ chars> c0mpute speech serve
TXT
