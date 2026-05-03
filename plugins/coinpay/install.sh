#!/usr/bin/env sh
# coinpay plugin installer.
#
# Convention: subprocess plugins install from <homepage>/install.sh, where
# <homepage> is the value of `homepage` in plugins/coinpay/module.toml.
# Routing through c0mpute.com (https://c0mpute.com/plugins/coinpay/install.sh)
# keeps the c0mpute install URL stable; the chain target tracks the
# manifest's homepage.
#
# Override via $COINPAY_INSTALL_URL for testing or local mirrors.
# Source: https://github.com/profullstack/c0mpute/tree/master/plugins/coinpay
set -eu

UPSTREAM="${COINPAY_INSTALL_URL:-https://coinpayportal.com/install.sh}"

# Probe upstream first so we can fail with a clean message rather than
# letting curl print "curl: (22)" into the user's terminal.
http_code=$(curl -sS -L -o /dev/null -w '%{http_code}' "$UPSTREAM" 2>/dev/null || echo "000")
if [ "$http_code" != "200" ]; then
  cat <<EOF >&2
✗ coinpay upstream installer not available yet.

  Tried: $UPSTREAM
  Status: HTTP $http_code

  CoinPay (https://coinpayportal.com) hasn't published its installer
  yet. coinpay is the DID + escrow + reputation layer for c0mpute and
  is required for worker registration; until it ships, c0mpute jobs
  that need a DID will be pre-launch.

  Track release availability at:
      https://github.com/profullstack/coinpayportal

EOF
  exit 1
fi

printf '\033[1;36m→\033[0m installing coinpay via %s\n' "$UPSTREAM"
exec sh -c "$(curl -fsSL "$UPSTREAM")" "$@"
