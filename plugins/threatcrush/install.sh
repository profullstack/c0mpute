#!/usr/bin/env sh
# threatcrush plugin installer for c0mpute nodes.
#
#   c0mpute plugin install threatcrush
#
# 1. Installs the ThreatCrush CLI/daemon if it is not on PATH
#    (https://threatcrush.com/install.sh; override $THREATCRUSH_INSTALL_URL).
# 2. Turns on the gateway access log: `[threatcrush] enabled = true` in the
#    c0mpute config of THIS user (run it as the user the worker runs as).
# 3. Registers that log with ThreatCrush: threatcrushd.conf.d/c0mpute.conf,
#    system-wide (/etc/threatcrush, needs root or sudo) when it exists or we
#    are root, otherwise the per-user daemon's ~/.threatcrush.
#
# Env: C0MPUTE_CONFIG (config.toml path), C0MPUTE_ACCESS_LOG (log path).
# Source: https://github.com/profullstack/c0mpute/tree/master/plugins/threatcrush
set -eu

say() { printf '\033[1;36m→\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }

# --- 1. the daemon ---------------------------------------------------------
if command -v threatcrush >/dev/null 2>&1; then
  say "threatcrush already installed ($(threatcrush --version 2>/dev/null | head -1 || echo unknown))"
else
  UPSTREAM="${THREATCRUSH_INSTALL_URL:-https://threatcrush.com/install.sh}"
  say "installing threatcrush via $UPSTREAM"
  curl -fsSL "$UPSTREAM" | sh
  if ! command -v threatcrush >/dev/null 2>&1; then
    warn "threatcrush is not on PATH yet; open a new shell, then re-run this installer"
    exit 1
  fi
fi

# --- 2. c0mpute config + log path (directories crate layout) ----------------
case "$(uname -s)" in
  Darwin)
    CFG_DIR="$HOME/Library/Application Support/com.c0mpute.c0mpute"
    DATA_DIR="$CFG_DIR"
    ;;
  *)
    CFG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/c0mpute"
    DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/c0mpute"
    ;;
esac
CONFIG="${C0MPUTE_CONFIG:-$CFG_DIR/config.toml}"
LOG="${C0MPUTE_ACCESS_LOG:-$DATA_DIR/gateway/access.log}"

mkdir -p "$(dirname "$CONFIG")" "$(dirname "$LOG")"
touch "$LOG"
if [ -f "$CONFIG" ] && grep -q '^\[threatcrush\]' "$CONFIG"; then
  if grep -A3 '^\[threatcrush\]' "$CONFIG" | grep -q '^enabled *= *true'; then
    say "c0mpute: [threatcrush] already enabled in $CONFIG"
  else
    warn "c0mpute: $CONFIG has a [threatcrush] section; set enabled = true in it yourself"
  fi
else
  cp "$CONFIG" "$CONFIG.bak-threatcrush" 2>/dev/null || true
  {
    printf '\n# Gateway access log for ThreatCrush (c0mpute plugin install threatcrush)\n'
    printf '[threatcrush]\nenabled = true\n'
    if [ -n "${C0MPUTE_ACCESS_LOG:-}" ]; then printf 'access_log = "%s"\n' "$LOG"; fi
  } >> "$CONFIG"
  say "c0mpute: enabled the gateway access log in $CONFIG"
fi

# --- 3. register the log with the daemon ------------------------------------
DROPIN="[log-watcher]
# Written by c0mpute plugin install threatcrush: the c0mpute gateway (:7777)
# access log, nginx vhost format.
paths = [\"$LOG\"]
"
SUDO=""
if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then SUDO="sudo"; fi
if [ -d /etc/threatcrush ] || [ "$(id -u)" -eq 0 ]; then
  CONFD=/etc/threatcrush/threatcrushd.conf.d
  if $SUDO mkdir -p "$CONFD" 2>/dev/null && printf '%s' "$DROPIN" | $SUDO tee "$CONFD/c0mpute.conf" >/dev/null 2>&1; then
    say "threatcrush: registered $LOG in $CONFD/c0mpute.conf"
  else
    warn "threatcrush: could not write $CONFD/c0mpute.conf (needs root). Contents:"
    printf '%s' "$DROPIN" >&2
  fi
else
  CONFD="$HOME/.threatcrush/threatcrushd.conf.d"
  mkdir -p "$CONFD"
  printf '%s' "$DROPIN" > "$CONFD/c0mpute.conf"
  say "threatcrush: registered $LOG in $CONFD/c0mpute.conf (user daemon; bans need root: sudo threatcrush install-service)"
fi

cat <<EOF

Done. Restart both so they pick it up:
  c0mpute worker:  systemctl --user restart c0mpute-worker   (or however you run it)
  threatcrush:     sudo systemctl restart threatcrushd       (or: threatcrush restart)

Needs threatcrush >= 0.13.77 for [log-watcher] paths. Check: threatcrush status
EOF
