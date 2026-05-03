#!/usr/bin/env sh
# c0mpute.com installer. Served at https://c0mpute.com/install.sh.
#
# Installs the c0mpute v1 stack idempotently:
#   mise      — runtime version manager (skipped if present)
#   bun       — JS runtime for TUI / JS-flavoured plugins (skipped if present)
#   c0mpute   — this repo (https://github.com/profullstack/c0mpute)
#   coinpay   — routed via https://c0mpute.com/plugins/coinpay/install.sh
#   infernet  — routed via https://c0mpute.com/plugins/infernet/install.sh
#
# Re-running upgrades in place. Each step skips when already installed
# unless --force is passed.
#
# Flags:
#   --minimal       Install only c0mpute (skip coinpay + infernet)
#   --no-coinpay    Skip CoinPay CLI
#   --no-infernet   Skip Infernet CLI
#   --worker        Add Docker / FFmpeg readiness checks
#   --developer     Verbose diagnostics
#   --force         Reinstall over existing
set -eu

C0MPUTE_VERSION="${C0MPUTE_VERSION:-latest}"
C0MPUTE_HOME="${C0MPUTE_HOME:-$HOME/.c0mpute}"
RELEASE_BASE="${C0MPUTE_RELEASE_BASE:-https://c0mpute.com/releases}"

# Route plugin installs through c0mpute.com so each wrapper can do its
# own error handling; the wrapper at https://c0mpute.com/plugins/<id>/install.sh
# is the in-repo plugins/<id>/install.sh and chains to upstream itself.
COINPAY_INSTALL_URL="${COINPAY_INSTALL_URL:-https://c0mpute.com/plugins/coinpay/install.sh}"
INFERNET_INSTALL_URL="${INFERNET_INSTALL_URL:-https://c0mpute.com/plugins/infernet/install.sh}"

INSTALL_C0MPUTE=1
INSTALL_COINPAY=1
INSTALL_INFERNET=1
WORKER_MODE=0
DEVELOPER_MODE=0
FORCE=0

say()  { printf '\033[1;36m→\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }
ok()   { printf '\033[1;32m✓\033[0m %s\n' "$*"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --minimal)      INSTALL_COINPAY=0; INSTALL_INFERNET=0 ;;
    --no-coinpay)   INSTALL_COINPAY=0 ;;
    --no-infernet)  INSTALL_INFERNET=0 ;;
    --worker)       WORKER_MODE=1 ;;
    --developer)    DEVELOPER_MODE=1 ;;
    --force)        FORCE=1 ;;
    --help|-h)
      sed -n '2,18p' "$0"
      exit 0
      ;;
    *)
      die "unknown flag: $1 (try --help)"
      ;;
  esac
  shift
done

detect_platform() {
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)
  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) die "unsupported arch: $arch" ;;
  esac
  case "$os" in
    linux|darwin) ;;
    *) die "unsupported os: $os (Linux/macOS only; Windows users see docs)" ;;
  esac
  printf '%s-%s' "$os" "$arch"
}

require() {
  command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"
}

# Detect the user's interactive shell. Tries (in order):
#   1. $SHELL — the login shell from passwd; what `chsh` sets.
#   2. /proc/$PPID/comm — the parent process name (best effort).
#   3. fall back to "sh".
# Returns one of: bash | zsh | fish | dash | ksh | sh
detect_shell() {
  candidate="${SHELL:-}"
  if [ -z "$candidate" ] && [ -r "/proc/$PPID/comm" ]; then
    candidate=$(cat "/proc/$PPID/comm" 2>/dev/null || true)
  fi
  case "${candidate:-}" in
    *zsh*)  echo "zsh"  ;;
    *bash*) echo "bash" ;;
    *fish*) echo "fish" ;;
    *dash*) echo "dash" ;;
    *ksh*)  echo "ksh"  ;;
    *)      echo "sh"   ;;
  esac
}

# Shell rc file path for a given shell name.
shell_rc_for() {
  case "$1" in
    bash) echo "$HOME/.bashrc" ;;
    zsh)  echo "$HOME/.zshrc"  ;;
    fish) echo "$HOME/.config/fish/config.fish" ;;
    *)    echo "$HOME/.profile" ;;
  esac
}

# ────────────────────────────────────────────────────────────────────────
# Idempotent mise + bun install
# ────────────────────────────────────────────────────────────────────────
#
# Some plugins are JS/TS-runtime-based (the Bun-built TUI, Node-based
# infernet variants). mise gives us a single tool to manage the runtime
# versions; bun is the JS runtime we standardise on. Both install
# user-locally — no sudo, no system package manager.

ensure_mise() {
  if command -v mise >/dev/null 2>&1; then
    return 0
  fi
  say "installing mise"
  if ! command -v curl >/dev/null 2>&1; then
    warn "curl missing; skipping mise install"
    return 1
  fi
  curl -fsSL https://mise.run | sh >/dev/null 2>&1 || {
    warn "mise install failed"
    return 1
  }
  if [ -x "$HOME/.local/bin/mise" ]; then
    PATH="$HOME/.local/bin:$PATH"
    export PATH
    ok "mise installed"
  fi
}

ensure_bun() {
  if command -v bun >/dev/null 2>&1; then
    return 0
  fi
  if command -v mise >/dev/null 2>&1; then
    say "installing bun via mise"
    mise use --global bun@latest >/dev/null 2>&1 || mise install bun@latest >/dev/null 2>&1 || true
    if [ -x "$HOME/.local/share/mise/installs/bun/latest/bin/bun" ] \
       || command -v bun >/dev/null 2>&1; then
      ok "bun installed (via mise)"
      return 0
    fi
  fi
  say "installing bun"
  curl -fsSL https://bun.sh/install | bash >/dev/null 2>&1 || {
    warn "bun install failed"
    return 1
  }
  if [ -x "$HOME/.bun/bin/bun" ]; then
    PATH="$HOME/.bun/bin:$PATH"
    export PATH
    ok "bun installed"
  fi
}

# ────────────────────────────────────────────────────────────────────────
# c0mpute itself
# ────────────────────────────────────────────────────────────────────────

install_c0mpute() {
  platform="$1"
  target="$C0MPUTE_HOME/bin/c0mpute"
  if [ -x "$target" ] && [ "$FORCE" -eq 0 ]; then
    # Verify the existing binary actually runs. v0.1.0's glibc-linked
    # build fails on systems with older glibc; v0.1.1+ uses musl. If
    # the binary can't execute `version` cleanly, treat it as broken
    # and force reinstall.
    if "$target" version >/dev/null 2>&1; then
      say "c0mpute already installed at $target (use --force to reinstall)"
      return 0
    fi
    warn "existing $target appears broken (won't run); reinstalling"
  fi

  artifact="c0mpute-${platform}.tar.gz"
  url="${RELEASE_BASE}/${C0MPUTE_VERSION}/${artifact}"
  sig_url="${url}.minisig"
  tmp=$(mktemp -d)

  say "downloading c0mpute ${C0MPUTE_VERSION}"
  http_code=$(curl -sSL -o "$tmp/$artifact" -w '%{http_code}' "$url" 2>/dev/null || echo "000")
  if [ "$http_code" != "200" ]; then
    rm -rf "$tmp"
    cat <<EOF >&2

✗ no prebuilt c0mpute binary at ${url} (HTTP ${http_code}).

  We don't have a release pipeline publishing binaries yet. While we
  set that up, install from source:

      git clone https://github.com/profullstack/c0mpute.git
      cd c0mpute
      cargo build --release --bin c0mpute
      mkdir -p ~/.c0mpute/bin
      cp target/release/c0mpute ~/.c0mpute/bin/c0mpute
      export PATH="\$HOME/.c0mpute/bin:\$PATH"

  Track release availability at:
      https://github.com/profullstack/c0mpute/releases

EOF
    exit 1
  fi
  curl -fsSL "$sig_url" -o "$tmp/$artifact.minisig" 2>/dev/null \
    || warn "no signature published for c0mpute yet; continuing"

  if command -v minisign >/dev/null 2>&1 && [ -f "$tmp/$artifact.minisig" ]; then
    say "verifying signature for c0mpute"
    C0MPUTE_PUBKEY="${C0MPUTE_PUBKEY:-RWQ_REPLACE_ME_WITH_PROD_MINISIGN_PUBKEY}"
    if ! minisign -V -P "$C0MPUTE_PUBKEY" -m "$tmp/$artifact" -x "$tmp/$artifact.minisig" >/dev/null 2>&1; then
      rm -rf "$tmp"
      die "signature verification failed for c0mpute"
    fi
  fi

  tar -xzf "$tmp/$artifact" -C "$C0MPUTE_HOME/bin"
  chmod +x "$C0MPUTE_HOME/bin/c0mpute"
  rm -rf "$tmp"
  ok "installed c0mpute → $C0MPUTE_HOME/bin/c0mpute"
}

# ────────────────────────────────────────────────────────────────────────
# Chain to upstream installers for coinpay + infernet
# ────────────────────────────────────────────────────────────────────────

chain_install() {
  name="$1"
  url="$2"

  if command -v "$name" >/dev/null 2>&1 && [ "$FORCE" -eq 0 ]; then
    say "$name already on PATH at $(command -v "$name") (use --force to reinstall)"
    return 0
  fi

  say "installing $name (via $url)"
  if ! curl -fsSL "$url" | sh; then
    warn "$name install failed (continuing without it)"
    return 1
  fi
}

# ────────────────────────────────────────────────────────────────────────
# PATH + diagnostics
# ────────────────────────────────────────────────────────────────────────

ensure_path() {
  # Bash / Zsh / sh-style profile. Each rc file gets the PATH line +
  # the right `mise activate <shell>` invocation. Idempotent — won't
  # duplicate lines on re-run.
  for rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
    [ -f "$rc" ] || continue

    if ! grep -q '\.c0mpute/bin' "$rc"; then
      {
        printf '\n# Added by c0mpute installer\n'
        printf 'export PATH="$HOME/.c0mpute/bin:$HOME/.local/bin:$PATH"\n'
      } >> "$rc"
    fi

    # mise activation per shell. .profile is sh-only and mise's
    # `activate sh` doesn't exist; we skip it there.
    if command -v mise >/dev/null 2>&1 && ! grep -q 'mise activate' "$rc"; then
      case "$rc" in
        *.bashrc) printf 'eval "$(mise activate bash)"\n' >> "$rc" ;;
        *.zshrc)  printf 'eval "$(mise activate zsh)"\n'  >> "$rc" ;;
      esac
    fi
  done

  # Fish — different syntax, different config path.
  fish_rc="$HOME/.config/fish/config.fish"
  if [ -d "$HOME/.config/fish" ] || [ "$(detect_shell)" = "fish" ]; then
    mkdir -p "$(dirname "$fish_rc")"
    [ -f "$fish_rc" ] || touch "$fish_rc"
    if ! grep -q '\.c0mpute/bin' "$fish_rc"; then
      {
        printf '\n# Added by c0mpute installer\n'
        printf 'fish_add_path -p $HOME/.c0mpute/bin $HOME/.local/bin\n'
      } >> "$fish_rc"
    fi
    if command -v mise >/dev/null 2>&1 && ! grep -q 'mise activate' "$fish_rc"; then
      printf 'mise activate fish | source\n' >> "$fish_rc"
    fi
  fi
}

print_versions() {
  echo
  if [ -x "$C0MPUTE_HOME/bin/c0mpute" ]; then
    printf 'c0mpute installed:  %s\n'  "$("$C0MPUTE_HOME/bin/c0mpute" version 2>/dev/null | tail -1)"
  fi
  if command -v coinpay >/dev/null 2>&1; then
    printf 'coinpay installed:  %s\n'  "$(coinpay --version 2>/dev/null || coinpay version 2>/dev/null | tail -1)"
  fi
  if command -v infernet >/dev/null 2>&1; then
    printf 'infernet installed: %s\n'  "$(infernet --version 2>/dev/null || infernet version 2>/dev/null | tail -1)"
  fi
}

run_doctor() {
  if [ -x "$C0MPUTE_HOME/bin/c0mpute" ]; then
    PATH="$C0MPUTE_HOME/bin:$PATH" "$C0MPUTE_HOME/bin/c0mpute" doctor || true
  fi
}

worker_checks() {
  echo
  say "worker-readiness checks"
  if command -v docker >/dev/null 2>&1; then ok "docker present"; else warn "docker not installed (recommended for sandboxed jobs)"; fi
  if command -v ffmpeg >/dev/null 2>&1; then ok "ffmpeg present"; else warn "ffmpeg not installed (required for transcode jobs)"; fi
}

# ────────────────────────────────────────────────────────────────────────
# main
# ────────────────────────────────────────────────────────────────────────

main() {
  require curl
  require tar
  require uname

  platform=$(detect_platform)
  mkdir -p "$C0MPUTE_HOME/bin"

  # Install runtime tooling some plugins need (idempotent — skipped if
  # already present). mise manages tool versions; bun runs the TUI and
  # any future JS-flavoured plugins.
  ensure_mise || true
  ensure_bun || true

  if [ "$INSTALL_C0MPUTE" -eq 1 ]; then install_c0mpute "$platform"; fi
  if [ "$INSTALL_COINPAY" -eq 1 ];  then chain_install coinpay  "$COINPAY_INSTALL_URL"; fi
  if [ "$INSTALL_INFERNET" -eq 1 ]; then chain_install infernet "$INFERNET_INSTALL_URL"; fi

  ensure_path

  if [ "$WORKER_MODE" -eq 1 ]; then worker_checks; fi

  print_versions
  run_doctor

  if ! printf '%s' "$PATH" | grep -q '\.c0mpute/bin'; then
    user_shell=$(detect_shell)
    rc_hint=$(shell_rc_for "$user_shell")
    printf '\n\033[1;33m! Your CURRENT %s shell does NOT have c0mpute on $PATH yet.\033[0m\n' "$user_shell"
    printf '  We added c0mpute + mise activation to your shell rc files,\n'
    printf '  but they only kick in for NEW shells.\n\n'
    printf '  Do ONE of these:\n'
    if [ "$user_shell" = "fish" ]; then
      printf '    \033[1;36msource %s\033[0m\n' "$rc_hint"
    else
      printf '    \033[1;36m. %s\033[0m\n' "$rc_hint"
    fi
    printf '    \033[1;36mexec $SHELL\033[0m                # restart current shell\n'
    printf '    Open a new terminal\n\n'
  fi

  cat <<EOF
Next steps:
  c0mpute coinpay did create
  c0mpute worker register
  c0mpute doctor
  c0mpute worker start

Docs: https://c0mpute.com/docs
EOF

  if [ "$DEVELOPER_MODE" -eq 1 ]; then
    echo
    say "developer mode: env"
    echo "  C0MPUTE_HOME=$C0MPUTE_HOME"
    echo "  C0MPUTE_VERSION=$C0MPUTE_VERSION"
    echo "  RELEASE_BASE=$RELEASE_BASE"
    echo "  COINPAY_INSTALL_URL=$COINPAY_INSTALL_URL"
    echo "  INFERNET_INSTALL_URL=$INFERNET_INSTALL_URL"
    echo "  PLATFORM=$platform"
  fi
}

main "$@"
