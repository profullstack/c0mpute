#!/usr/bin/env sh
# nixamp plugin installer for c0mpute nodes.
#
#   c0mpute plugin install nixamp
#
# 1. Installs the nixamp CLI if it is not on PATH
#    (https://nixamp.com/install.sh; override $NIXAMP_INSTALL_URL).
# 2. Checks for ffmpeg and ffprobe, which nixamp needs to play and broadcast.
# 3. Prints how to start a station and put it on c0mpute Live.
#
# It starts nothing and does not touch the c0mpute config. Run it as the user
# that will run the station, never as root.
# Source: https://github.com/profullstack/c0mpute/tree/master/plugins/nixamp
set -eu

say() { printf '\033[1;36m→\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }

if [ "$(id -u)" -eq 0 ]; then
  warn "running as root: nixamp will be installed for root. Run this as the user that runs your node instead."
fi

# --- 1. the CLI ------------------------------------------------------------
if command -v nixamp >/dev/null 2>&1; then
  say "nixamp already installed ($(nixamp --version 2>/dev/null | head -1 || echo unknown))"
else
  UPSTREAM="${NIXAMP_INSTALL_URL:-https://nixamp.com/install.sh}"
  say "installing nixamp via $UPSTREAM"
  # Download to a file, then run it: never pipe the network into a shell, so a
  # truncated or failed transfer cannot execute half a script.
  TMP_INSTALL="$(mktemp)"
  trap 'rm -f "$TMP_INSTALL"' EXIT
  curl -fsSL --proto '=https' --tlsv1.2 -o "$TMP_INSTALL" "$UPSTREAM"
  sh "$TMP_INSTALL"
  rm -f "$TMP_INSTALL"
  if ! command -v nixamp >/dev/null 2>&1; then
    if [ -x "$HOME/.local/bin/nixamp" ]; then
      warn "nixamp is installed at $HOME/.local/bin/nixamp, which is not on PATH in this shell; open a new shell"
    else
      warn "nixamp is not on PATH yet; open a new shell, then re-run this installer"
      exit 1
    fi
  fi
fi

# --- 2. what it needs -------------------------------------------------------
MISSING=""
for bin in ffmpeg ffprobe; do
  command -v "$bin" >/dev/null 2>&1 || MISSING="$MISSING $bin"
done
if [ -n "$MISSING" ]; then
  warn "missing:$MISSING. nixamp cannot play or broadcast without them (Debian/Ubuntu: sudo apt install ffmpeg; macOS: brew install ffmpeg)"
fi

# --- 3. next steps ----------------------------------------------------------
cat <<'EOF'

Done. Nothing was started. To run a station from a folder of music:

  nixamp daemon start ~/Music --name "my station" --announce

To put that station on c0mpute Live, so c0mpute nodes relay it and anyone
watches at a c0mpute.com/live link:

  1. Create a stream and copy its key (shown once):
       https://c0mpute.com/dashboard/live
  2. Start the station with the key as a destination:
       nixamp daemon start ~/Music --name "my station" --announce --rtmp c0mpute=<stream key>
  3. Start the broadcast from the station's admin page, or:
       curl -X POST -H "x-nixamp-key: <control key>" http://127.0.0.1:4321/api/broadcast

Needs nixamp with the c0mpute destination (see `nixamp --help`; update with
`nixamp update`). Relays are paid $0.01 per GB from the streamer's c0mpute
balance; with no balance the stream runs on the free tier.

Keep it running across reboots with your own service manager, for example a
user crontab line:  */5 * * * * nixamp daemon status >/dev/null 2>&1 || nixamp daemon restart
EOF
