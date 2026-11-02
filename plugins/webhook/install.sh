#!/usr/bin/env sh
# webhook plugin installer.
#
# webhook is an in-process plugin compiled into the `c0mpute` binary itself
# (c0mpute >= 0.2.53), so there is nothing extra to install.
#
# Served at https://c0mpute.com/plugins/webhook/install.sh.
set -eu

ok()   { printf '\033[1;32m✓\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }

if command -v c0mpute >/dev/null 2>&1; then
  ok "webhook is built into c0mpute ($(c0mpute --version 2>/dev/null || echo installed))"
else
  warn "c0mpute is not installed: curl -fsSL https://c0mpute.com/install.sh | sh"
fi
cat <<'TXT'

  Worker (earn per delivery):  c0mpute webhook serve --price 0.001
  Buyer (send one webhook):    c0mpute webhook send https://example.com/hook \
                                 -d '{"event":"paid"}' --secret whsec_...
TXT
