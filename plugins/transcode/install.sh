#!/usr/bin/env sh
# transcode plugin installer.
#
# transcode is an in-process plugin compiled into the `c0mpute` binary
# itself, so there's no separate transcode binary to install. What this
# script *does* install is the only thing transcode needs from the host:
# ffmpeg.
#
# Same package-manager pattern as the c0mpute root installer
# (apt/dnf/yum/pacman/zypper/apk on Linux, brew on macOS). Routes sudo's
# stdin/stderr through /dev/tty so it works under `curl | sh`.
#
# Served at https://c0mpute.com/plugins/transcode/install.sh.
set -eu

say()  { printf '\033[1;36m→\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
ok()   { printf '\033[1;32m✓\033[0m %s\n' "$*"; }

ensure_ffmpeg() {
  if command -v ffmpeg >/dev/null 2>&1; then
    ok "ffmpeg present ($(ffmpeg -version 2>/dev/null | head -1 | awk '{print $1, $2, $3}'))"
    return 0
  fi

  os=$(uname -s)

  # macOS — Homebrew
  if [ "$os" = "Darwin" ]; then
    if command -v brew >/dev/null 2>&1; then
      say "installing ffmpeg via brew"
      brew install ffmpeg && { ok "ffmpeg installed"; return 0; }
    fi
    warn "ffmpeg missing; install via: brew install ffmpeg"
    return 1
  fi

  # Pick a sudo prefix: needed unless we're already root. If we're piped
  # over curl|sh the controlling tty is /dev/tty, so route sudo's
  # password prompt there.
  if [ "$(id -u 2>/dev/null || echo 1)" = "0" ]; then
    SUDO=""
  elif command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
  else
    warn "ffmpeg missing and no sudo available; install ffmpeg manually"
    return 1
  fi

  # Package managers may prompt (sudo password, conffiles), so give them the
  # terminal when there is one. Under `curl | sh` over ssh without -t, in
  # cloud-init or in a container there is no /dev/tty, and redirecting to it
  # fails before the package manager even runs; install non-interactively.
  if ( : </dev/tty ) 2>/dev/null; then
    run_pm() { "$@" </dev/tty 2>/dev/tty; }
  else
    run_pm() { "$@" </dev/null; }
  fi

  if command -v apt-get >/dev/null 2>&1; then
    say "installing ffmpeg via apt-get"
    if ! dpkg -l 2>/dev/null | grep -q '^ii  ffmpeg '; then
      $SUDO apt-get update -y >/dev/null 2>&1 || true
      run_pm $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y ffmpeg && { ok "ffmpeg installed"; return 0; }
    fi
  elif command -v dnf >/dev/null 2>&1; then
    say "installing ffmpeg via dnf"
    run_pm $SUDO dnf install -y ffmpeg && { ok "ffmpeg installed"; return 0; }
  elif command -v yum >/dev/null 2>&1; then
    say "installing ffmpeg via yum"
    run_pm $SUDO yum install -y ffmpeg && { ok "ffmpeg installed"; return 0; }
  elif command -v pacman >/dev/null 2>&1; then
    say "installing ffmpeg via pacman"
    run_pm $SUDO pacman -S --noconfirm ffmpeg && { ok "ffmpeg installed"; return 0; }
  elif command -v zypper >/dev/null 2>&1; then
    say "installing ffmpeg via zypper"
    run_pm $SUDO zypper install -y ffmpeg && { ok "ffmpeg installed"; return 0; }
  elif command -v apk >/dev/null 2>&1; then
    say "installing ffmpeg via apk"
    run_pm $SUDO apk add --no-cache ffmpeg && { ok "ffmpeg installed"; return 0; }
  else
    warn "no supported package manager found; install ffmpeg manually"
    return 1
  fi

  warn "ffmpeg install failed; transcode jobs won't run until you install it"
  return 1
}

ensure_ffmpeg

cat <<EOF

transcode is built into the c0mpute binary — no separate plugin binary
to install. ffmpeg is the only system dependency.

Submit a transcode job:
  c0mpute transcode submit input.mov --preset hls
EOF
