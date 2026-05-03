#!/usr/bin/env sh
# depin.quest CLI installer. Served at https://depin.quest/video/install.sh.
# Installs the `depin` binary; today the only product line is `depin video`
# (Quest), but the same binary picks up future lines automatically.
# Idempotent — re-running upgrades in place.
set -eu

DEPIN_VERSION="${DEPIN_VERSION:-latest}"
DEPIN_HOME="${DEPIN_HOME:-$HOME/.depin}"
RELEASE_BASE="${DEPIN_RELEASE_BASE:-https://depin.quest/video/releases}"

say() { printf '\033[1;36m→\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

detect_platform() {
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)
  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) die "unsupported arch: $arch" ;;
  esac
  case "$os" in
    linux) ;;
    darwin) ;;
    *) die "unsupported os: $os (Linux/macOS only; Windows users see docs)" ;;
  esac
  printf '%s-%s' "$os" "$arch"
}

require() {
  command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"
}

main() {
  require curl
  require tar
  require uname

  platform=$(detect_platform)
  artifact="depin-${platform}.tar.gz"
  url="${RELEASE_BASE}/${DEPIN_VERSION}/${artifact}"
  sig_url="${url}.minisig"

  mkdir -p "$DEPIN_HOME/bin"
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT

  say "downloading depin ${DEPIN_VERSION} for ${platform}"
  curl -fsSL "$url" -o "$tmp/$artifact"
  curl -fsSL "$sig_url" -o "$tmp/$artifact.minisig" || warn "no signature found; continuing"

  if command -v minisign >/dev/null 2>&1 && [ -f "$tmp/$artifact.minisig" ]; then
    say "verifying signature"
    # Embedded public key (rotated per PRD §16). Replace before each release.
    DEPIN_PUBKEY="${DEPIN_PUBKEY:-RWQ_REPLACE_ME_WITH_PROD_MINISIGN_PUBKEY}"
    if ! minisign -V -P "$DEPIN_PUBKEY" -m "$tmp/$artifact" -x "$tmp/$artifact.minisig" >/dev/null 2>&1; then
      die "signature verification failed for $artifact"
    fi
  else
    warn "minisign not installed; signature NOT verified (recommend: install minisign and re-run)"
  fi

  say "extracting"
  tar -xzf "$tmp/$artifact" -C "$DEPIN_HOME/bin"
  chmod +x "$DEPIN_HOME/bin/depin"

  for rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
    [ -f "$rc" ] || continue
    if ! grep -q '\.depin/bin' "$rc"; then
      printf '\n# Added by depin installer\nexport PATH="$HOME/.depin/bin:$PATH"\n' >> "$rc"
    fi
  done

  say "running depin video doctor"
  "$DEPIN_HOME/bin/depin" video doctor || warn "doctor reported issues; review above"

  cat <<EOF

depin installed at $DEPIN_HOME/bin/depin

Next steps:
  1. Reload your shell, or:
       export PATH="\$HOME/.depin/bin:\$PATH"
  2. Get an API token at https://depin.quest/video/app/provider and:
       depin video config set api.token <YOUR_TOKEN>
  3. Start earning:
       depin video start --roles storage,transcode,gateway

Docs: https://depin.quest/video/docs
EOF
}

main "$@"
