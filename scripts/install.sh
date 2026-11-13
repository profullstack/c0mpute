#!/usr/bin/env sh
# c0mpute.com installer. Served at https://c0mpute.com/install.sh.
#
# Installs the c0mpute v1 stack idempotently:
#   c0mpute   — this repo (https://github.com/profullstack/c0mpute), signed release
#   coinpay   — plugins/coinpay/install.sh from the signed c0mpute-plugins.tar.gz
#   infernet  — plugins/infernet/install.sh from the signed c0mpute-plugins.tar.gz
#   tui       — c0mpute-tui from the signed c0mpute-tui.tar.gz
#   bun       — pinned JS runtime for the TUI, checked by sha256 (skipped if present)
#
# Re-running upgrades in place. Each step skips when already installed
# unless --force is passed.
#
# Flags:
#   --minimal       Install only c0mpute (skip coinpay + infernet + transcode deps)
#   --no-coinpay    Skip CoinPay CLI
#   --no-infernet   Skip Infernet CLI
#   --no-transcode  Skip transcode plugin's system deps (ffmpeg)
#   --no-tui        Skip the c0mpute-tui terminal UI
#   --desktop       Also install the desktop app (Linux AppImage; chat, agent,
#                   node manager, hosting) as ~/.local/bin/c0mpute-desktop
#   --worker        Add Docker / FFmpeg readiness checks
#   --developer     Verbose diagnostics
#   --force         Reinstall over existing
#   --ref <code>    Credit a referrer on c0mpute.com/nodes (same as C0MPUTE_REF)
#   --name <handle> Your node's nickname on c0mpute.com/nodes (same as C0MPUTE_NAME)
#   --no-house-models       Don't pull Infernet house models (same as
#                           INFERNET_NO_HOUSE_MODELS=1). They are ON by default:
#                           the node pulls and serves the models listed at
#                           https://infernetprotocol.com/house-models.json that fit it.
#   --uncensored-models     Also serve uncensored house models. They are OFF
#                           unless you opt in (same as INFERNET_UNCENSORED_MODELS=1)
#   --no-uncensored-models  Keep uncensored house models off
#                           (same as INFERNET_NO_UNCENSORED_MODELS=1)
#   --system-service        Run the worker as a root-managed systemd system
#                           service (needs sudo). Default: a systemd *user*
#                           service under your account, no root involved.
#   --no-exec       Don't auto-exec a fresh shell at the end
#                   (CI-friendly; default is to drop you into a new
#                   shell with c0mpute already on $PATH)
#
# Env vars:
#   NO_STORAGE_RELOCATE=1   Don't symlink ~/.local/share/c0mpute to a
#                           bigger volume even if one's available
#                           (default: relocate when a writable mount
#                           has ≥2x more free space than $HOME)
set -eu

C0MPUTE_VERSION="${C0MPUTE_VERSION:-latest}"
C0MPUTE_HOME="${C0MPUTE_HOME:-$HOME/.c0mpute}"
RELEASE_BASE="${C0MPUTE_RELEASE_BASE:-https://c0mpute.com/releases}"

# Plugin installers (the in-repo plugins/<id>/install.sh, which chain to
# upstream themselves) run from the release's signed c0mpute-plugins.tar.gz.
# These URLs serve the same files unsigned and are only used for a release
# from before that archive existed (see UNSIGNED_EXTRAS_UNTIL).
COINPAY_INSTALL_URL="${COINPAY_INSTALL_URL:-https://c0mpute.com/plugins/coinpay/install.sh}"
INFERNET_INSTALL_URL="${INFERNET_INSTALL_URL:-https://c0mpute.com/plugins/infernet/install.sh}"
TRANSCODE_INSTALL_URL="${TRANSCODE_INSTALL_URL:-https://c0mpute.com/plugins/transcode/install.sh}"

# Last release WITHOUT the signed extras (c0mpute-plugins.tar.gz,
# c0mpute-tui.tar.gz, AppImage .minisig). Only a release this old may fall back
# to the unverified downloads when an extra is absent; on anything newer a
# missing extra is an error, so hiding the asset cannot force the fallback.
UNSIGNED_EXTRAS_UNTIL="0.3.64"
# The release everything in this run comes from (v<x.y.z>): the version in the
# binary's signed comment, or the already-installed binary's. "" = unknown.
RELEASE_TAG=""
SIGNED_VERSION=""
SCRATCH=""

INSTALL_C0MPUTE=1
INSTALL_COINPAY=1
INSTALL_INFERNET=1
INSTALL_TRANSCODE=1
INSTALL_TUI=1
INSTALL_DESKTOP="${C0MPUTE_DESKTOP:-0}"
WORKER_MODE=0
DEVELOPER_MODE=0
FORCE=0
NO_START=0
SYSTEM_SERVICE="${C0MPUTE_SYSTEM_SERVICE:-0}"

say()  { printf '\033[1;36m→\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }
ok()   { printf '\033[1;32m✓\033[0m %s\n' "$*"; }

# Privilege escalation helper for system-wide steps (the /etc/profile.d hook,
# apt, a --system-service unit). Empty when already root or when sudo isn't
# available; callers try a direct write first and fall back to $SUDO.
if [ "$(id -u 2>/dev/null || echo 0)" != "0" ] && command -v sudo >/dev/null 2>&1; then
  SUDO="sudo"
else
  SUDO=""
fi

while [ $# -gt 0 ]; do
  case "$1" in
    --minimal)      INSTALL_COINPAY=0; INSTALL_INFERNET=0; INSTALL_TRANSCODE=0; INSTALL_TUI=0 ;;
    --no-coinpay)   INSTALL_COINPAY=0 ;;
    --no-infernet)  INSTALL_INFERNET=0 ;;
    --no-transcode) INSTALL_TRANSCODE=0 ;;
    --no-tui)       INSTALL_TUI=0 ;;
    --desktop)      INSTALL_DESKTOP=1 ;;
    --no-start)     NO_START=1 ;;
    --worker)       WORKER_MODE=1 ;;
    --developer)    DEVELOPER_MODE=1 ;;
    --force)        FORCE=1 ;;
    --no-exec)      NO_EXEC=1 ;;
    --ref)          shift; C0MPUTE_REF="${1:-}" ;;
    --ref=*)        C0MPUTE_REF="${1#--ref=}" ;;
    --name)         shift; C0MPUTE_NAME="${1:-}" ;;
    --name=*)       C0MPUTE_NAME="${1#--name=}" ;;
    --no-house-models)      INFERNET_NO_HOUSE_MODELS=1 ;;
    --uncensored-models)    INFERNET_UNCENSORED_MODELS=1 ;;
    --no-uncensored-models) INFERNET_NO_UNCENSORED_MODELS=1 ;;
    --system-service)       SYSTEM_SERVICE=1 ;;
    --help|-h)
      sed -n '2,41p' "$0"
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
# bun (pinned)
# ────────────────────────────────────────────────────────────────────────
#
# The TUI (c0mpute-tui) is TypeScript run through bun; nothing else on an
# installed node needs a JS runtime, so bun is only installed with the TUI.
# We download the pinned upstream release zip for this OS/arch (not bun's
# installer script) and check it against a hard-coded sha256 before anything
# in it runs, the same way minisign is pinned below. User-local, no sudo.
#
# mise is NOT installed any more: it is the contributor toolchain manager
# (.mise.toml, scripts/dev-setup.sh), not something a node needs at runtime.
#
# Hashes: upstream's SHASUMS256.txt for bun-v1.3.14
# (github.com/oven-sh/bun/releases), each re-computed from the downloaded zip.
# Bump the version and every hash together.
BUN_PIN_VERSION="1.3.14"
bun_pin_sha256() {
  case "$1" in
    linux-x64)               echo 951ee2aee855f08595aeec6225226a298d3fea83a3dcd6465c09cbccdf7e848f ;;
    linux-x64-baseline)      echo a063908ae08b7852ca10939bbdc6ceed3ddabce8fb9402dce83d65d73b36e6c7 ;;
    linux-x64-musl)          echo 14bd9aedeebf1dba67e8def9531c89bc989ecfdf1de42e5bfcaf1b8cd9294719 ;;
    linux-x64-musl-baseline) echo 56a7d6806cf155536c0178f0ea5fbd098e684fa509ebdb4fc0a7e19fb65382dc ;;
    linux-aarch64)           echo a27ffb63a8310375836e0d6f668ae17fa8d8d18b88c37c821c65331973a19a3b ;;
    linux-aarch64-musl)      echo b98e0ad3625c5c00d1d5b5ff55605c7adddbfae151861e68ade57b2d3b8703bb ;;
    darwin-x64)              echo 4183df3374623e5bab315c547cfa0974533cd457d86b73b639f7a87974cd6633 ;;
    darwin-x64-baseline)     echo 3e35ad6f53971a9834bf9e6786e2adf72b5f1921cc9a9c5fde073d2972944076 ;;
    darwin-aarch64)          echo d8b96221828ad6f97ac7ac0ab7e95872341af763001e8803e8267652c2652620 ;;
  esac
}

# The bun release build for platform $1 on this machine (prints nothing: none).
bun_target() {
  case "$1" in
    linux-x86_64)   bt="linux-x64" ;;
    linux-aarch64)  bt="linux-aarch64" ;;
    darwin-aarch64) bt="darwin-aarch64" ;;
    darwin-x86_64)
      bt="darwin-x64"
      # An x86_64 shell under Rosetta: the machine is Apple silicon.
      if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = "1" ]; then bt="darwin-aarch64"; fi
      ;;
    *) return 0 ;;
  esac
  case "$bt" in
    linux-*)
      # musl libc (Alpine) needs the musl build.
      if [ -f /etc/alpine-release ] || { ldd --version 2>&1 || true; } | grep -qi musl; then
        bt="$bt-musl"
      fi
      ;;
  esac
  # x64 CPUs without AVX2 need the baseline build (it runs on every x64).
  case "$bt" in
    linux-x64*) grep -qw avx2 /proc/cpuinfo 2>/dev/null || bt="$bt-baseline" ;;
    darwin-x64) [ "$(sysctl -n hw.optional.avx2_0 2>/dev/null || true)" = "1" ] || bt="$bt-baseline" ;;
  esac
  printf '%s' "$bt"
}

ensure_bun() { # <platform>
  if command -v bun >/dev/null 2>&1; then
    return 0
  fi
  if [ -x "$HOME/.bun/bin/bun" ]; then
    PATH="$HOME/.bun/bin:$PATH"
    export PATH
    return 0
  fi
  bun_manual="install bun ${BUN_PIN_VERSION} yourself (https://bun.sh/docs/installation), then run: c0mpute tui"
  bt=$(bun_target "$1")
  bun_want=$(bun_pin_sha256 "$bt")
  if [ -z "$bt" ] || [ -z "$bun_want" ]; then
    warn "no pinned bun build for ${bt:-$1}; $bun_manual"
    return 1
  fi
  if ! command -v unzip >/dev/null 2>&1; then
    warn "unzip is needed to unpack bun: install unzip and re-run, or $bun_manual"
    return 1
  fi
  say "installing bun ${BUN_PIN_VERSION} (${bt}, pinned by sha256)"
  bun_dir="$SCRATCH/bun"
  mkdir -p "$bun_dir"
  curl -fsSL -o "$bun_dir/bun.zip" \
    "${C0MPUTE_BUN_BASE:-https://github.com/oven-sh/bun/releases/download}/bun-v${BUN_PIN_VERSION}/bun-${bt}.zip" 2>/dev/null \
    || { warn "could not download bun; $bun_manual"; return 1; }
  bun_got=$(c0mpute_sha256 "$bun_dir/bun.zip") || { warn "no sha256 tool (sha256sum/shasum/openssl) to check bun"; return 1; }
  if [ "$bun_got" != "$bun_want" ]; then
    rm -f "$bun_dir/bun.zip"
    warn "bun download has sha256 ${bun_got}, expected ${bun_want}; not installing it"
    return 1
  fi
  unzip -q -o "$bun_dir/bun.zip" "bun-${bt}/bun" -d "$bun_dir" >/dev/null 2>&1 \
    || { warn "could not unzip bun"; return 1; }
  mkdir -p "$HOME/.bun/bin"
  { mv -f "$bun_dir/bun-${bt}/bun" "$HOME/.bun/bin/bun" && chmod 755 "$HOME/.bun/bin/bun"; } \
    || { warn "could not install bun into $HOME/.bun/bin"; return 1; }
  PATH="$HOME/.bun/bin:$PATH"
  export PATH
  ok "bun ${BUN_PIN_VERSION} installed → $HOME/.bun/bin/bun"
}

# ────────────────────────────────────────────────────────────────────────
# c0mpute itself
# ────────────────────────────────────────────────────────────────────────

# Tags of the most recent published releases, newest first (drafts are not
# listed). Empty when GitHub's API is unreachable or rate limited.
recent_release_tags() {
  curl -fsSL -H 'Accept: application/vnd.github+json' \
    "${C0MPUTE_RELEASES_API:-https://api.github.com/repos/profullstack/c0mpute/releases?per_page=6}" 2>/dev/null \
    | sed -n 's/^ *"tag_name": *"\([^"]*\)".*/\1/p' | head -n 6
}

# Release signing. Every artifact on a c0mpute release has a minisign
# signature next to it (<artifact>.minisig). The public key is also committed
# as minisign.pub at the repo root; C0MPUTE_PUBKEY overrides it for testing.
C0MPUTE_PUBKEY="${C0MPUTE_PUBKEY:-RWQqOew3QbWhAWH36TA5roreh18RgRqgozSwhE3yZtdza7eeLKmbDbJM}"
# When minisign isn't installed we fetch this pinned upstream release
# (github.com/jedisct1/minisign, static binaries) into the temp dir and check
# it against these hard-coded sha256s before running it. No root needed.
MINISIGN_PIN_VERSION="0.12"
MINISIGN_PIN_LINUX_SHA256="9a599b48ba6eb7b1e80f12f36b94ceca7c00b7a5173c95c3efc88d9822957e73"
MINISIGN_PIN_MACOS_SHA256="89000b19535765f9cffc65a65d64a820f433ef6db8020667f7570e06bf6aac63"

c0mpute_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 -r "$1" | cut -d' ' -f1
  else return 1
  fi
}

# Print the path of a minisign binary for platform $1, fetching the pinned
# release into dir $2 when none is installed. Fails when neither works.
c0mpute_minisign() {
  if [ -z "${C0MPUTE_FORCE_PINNED_MINISIGN:-}" ] && command -v minisign >/dev/null 2>&1; then
    command -v minisign
    return 0
  fi
  case "$1" in
    linux-x86_64)   ms_asset="minisign-${MINISIGN_PIN_VERSION}-linux.tar.gz"; ms_want="$MINISIGN_PIN_LINUX_SHA256"; ms_member="minisign-linux/x86_64/minisign" ;;
    linux-aarch64)  ms_asset="minisign-${MINISIGN_PIN_VERSION}-linux.tar.gz"; ms_want="$MINISIGN_PIN_LINUX_SHA256"; ms_member="minisign-linux/aarch64/minisign" ;;
    darwin-aarch64) ms_asset="minisign-${MINISIGN_PIN_VERSION}-macos.zip";    ms_want="$MINISIGN_PIN_MACOS_SHA256"; ms_member="minisign" ;;
    *) warn "no pinned minisign build for $1"; return 1 ;;
  esac
  say "minisign not installed; fetching pinned minisign ${MINISIGN_PIN_VERSION} to verify the download" >&2
  curl -fsSL -o "$2/$ms_asset" "https://github.com/jedisct1/minisign/releases/download/${MINISIGN_PIN_VERSION}/${ms_asset}" 2>/dev/null \
    || { warn "could not download minisign"; return 1; }
  ms_got=$(c0mpute_sha256 "$2/$ms_asset") || { warn "no sha256 tool (sha256sum/shasum/openssl) to check minisign"; return 1; }
  if [ "$ms_got" != "$ms_want" ]; then
    warn "minisign download has sha256 ${ms_got}, expected ${ms_want}"
    return 1
  fi
  mkdir -p "$2/minisign"
  case "$ms_asset" in
    *.zip) unzip -q -o "$2/$ms_asset" "$ms_member" -d "$2/minisign" >/dev/null 2>&1 || { warn "could not unzip minisign"; return 1; } ;;
    *) tar -xzf "$2/$ms_asset" -C "$2/minisign" "$ms_member" 2>/dev/null || [ -f "$2/minisign/$ms_member" ] || { warn "could not unpack minisign"; return 1; } ;;
  esac
  chmod +x "$2/minisign/$ms_member"
  printf '%s\n' "$2/minisign/$ms_member"
}

# True when dotted version $1 is lower than $2 (a leading v is ignored).
version_lt() {
  vl_a="${1#v}"; vl_b="${2#v}"
  [ "$vl_a" != "$vl_b" ] || return 1
  vl_low=$(printf '%s\n%s\n' "$vl_a" "$vl_b" | sort -t. -k1,1n -k2,2n -k3,3n | head -n 1)
  [ "$vl_low" = "$vl_a" ]
}

# x.y.z of the c0mpute already on this machine ("" when none, or it won't run).
installed_c0mpute_version() {
  icv_bin="$C0MPUTE_HOME/bin/c0mpute"
  [ -x "$icv_bin" ] || icv_bin=$(command -v c0mpute 2>/dev/null || true)
  [ -n "$icv_bin" ] || return 0
  "$icv_bin" --version 2>/dev/null \
    | sed -n 's/^c0mpute  *v\{0,1\}\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\).*/\1/p' | head -n 1
}

# Verify a downloaded release asset against its minisign signature.
#   $1 platform   $2 the file ($2.minisig next to it)   $3 its name on the release
#   $4 the release tag the signature must be for ("" = any: the `latest` URL)
# Fails closed: no verifier, a bad signature, or a valid signature for another
# file or release all abort the install. Sets SIGNED_VERSION to the release tag
# in the signed (trusted) comment, `timestamp:<unix> version:<tag> file:<name>`.
c0mpute_verify() {
  if [ -z "${MS_BIN:-}" ] || [ ! -x "$MS_BIN" ]; then
    MS_BIN=$(c0mpute_minisign "$1" "$SCRATCH") \
      || die "cannot verify the download: install minisign (brew install minisign / apt install minisign / https://jedisct1.github.io/minisign/) and re-run"
  fi
  ms_out=$("$MS_BIN" -V -P "$C0MPUTE_PUBKEY" -m "$2" -x "$2.minisig" 2>&1) \
    || die "signature verification FAILED for $3; not installing it"
  ms_comment=$(printf '%s\n' "$ms_out" | sed -n 's/^Trusted comment: //p' | head -n 1)
  [ -n "$ms_comment" ] || ms_comment="$ms_out"
  # The signed comment names the file and release, so a validly signed
  # artifact for another platform or release can't be swapped in.
  case " $ms_comment " in
    *" file:$3 "*) ;;
    *) die "signature for $3 is valid but signs a different file; not installing it" ;;
  esac
  if [ -n "$4" ]; then
    case " $ms_comment " in
      *" version:$4 "*) ;;
      *) die "signature for $3 is valid but not for release $4; not installing it" ;;
    esac
  fi
  SIGNED_VERSION=$(printf '%s\n' "$ms_comment" | tr ' ' '\n' | sed -n 's/^version://p' | head -n 1)
  case "$SIGNED_VERSION" in
    *[!A-Za-z0-9._-]*) die "signature for $3 carries a malformed release version; not installing it" ;;
  esac
}

# True when the release being installed is from before the signed extras
# existed, so an absent one is expected rather than suspicious. An unknown
# release is not: only C0MPUTE_ALLOW_UNSIGNED=1 gets the old behaviour then.
legacy_release() {
  if [ -z "$RELEASE_TAG" ]; then
    [ "${C0MPUTE_ALLOW_UNSIGNED:-0}" = "1" ]
    return
  fi
  ! version_lt "$UNSIGNED_EXTRAS_UNTIL" "$RELEASE_TAG"
}

# Download release asset $2 for platform $1 from the same release as the
# binary and verify it exactly like the binary. Sets ASSET_PATH. Returns
#   0  downloaded and verified
#   2  the release does not have it (HTTP 404) and predates signed extras: the
#      only case where a caller may fall back to the old unverified download
#   1  could not be fetched, or missing from a release that must have it
# An asset that IS there with a missing or bad signature aborts the install.
fetch_signed_asset() {
  fa_url="${RELEASE_BASE}/${RELEASE_TAG:-latest}/$2"
  mkdir -p "$SCRATCH/assets"
  ASSET_PATH="$SCRATCH/assets/$2"
  fa_code=$(curl -sSL -o "$ASSET_PATH" -w '%{http_code}' "$fa_url" 2>/dev/null || echo "000")
  if [ "$fa_code" = "404" ]; then
    rm -f "$ASSET_PATH"
    if legacy_release; then return 2; fi
    warn "$2 is missing from the ${RELEASE_TAG:-latest} release, which should carry it (${fa_url})"
    return 1
  fi
  if [ "$fa_code" != "200" ]; then
    rm -f "$ASSET_PATH"
    warn "could not download ${fa_url} (HTTP ${fa_code})"
    return 1
  fi
  fa_sig=$(curl -sSL -o "$ASSET_PATH.minisig" -w '%{http_code}' "${fa_url}.minisig" 2>/dev/null || echo "000")
  if [ "$fa_sig" != "200" ]; then
    rm -f "$ASSET_PATH.minisig"
    if [ "${C0MPUTE_ALLOW_UNSIGNED:-0}" = "1" ]; then
      warn "no signature at ${fa_url}.minisig (HTTP ${fa_sig}); using $2 UNVERIFIED because C0MPUTE_ALLOW_UNSIGNED=1"
      return 0
    fi
    die "no signature at ${fa_url}.minisig (HTTP ${fa_sig}); refusing to use an unverified $2 (C0MPUTE_ALLOW_UNSIGNED=1 overrides)"
  fi
  say "verifying signature for $2"
  c0mpute_verify "$1" "$ASSET_PATH" "$2" "$RELEASE_TAG"
  ok "signature verified: $2 (${SIGNED_VERSION})"
}

# The checkout this script was run from, when it was run as a file from one
# (sh scripts/install.sh). Empty under `curl | sh`: there $0 is the shell, and
# a path relative to it would be whatever directory you happen to be in.
repo_root() {
  case "$0" in */install.sh) ;; *) return 0 ;; esac
  rr=$(cd "$(dirname "$0")/.." 2>/dev/null && pwd) || return 0
  if [ -f "$rr/scripts/install.sh" ] && [ -d "$rr/plugins" ]; then printf '%s' "$rr"; fi
}

install_c0mpute() {
  platform="$1"
  target="$C0MPUTE_HOME/bin/c0mpute"
  have_ver=$(installed_c0mpute_version)
  if [ -x "$target" ] && [ "$FORCE" -eq 0 ]; then
    # Verify the existing binary actually runs. v0.1.0's glibc-linked
    # build fails on systems with older glibc; v0.1.1+ uses musl. If
    # the binary can't execute `version` cleanly, treat it as broken
    # and force reinstall.
    if "$target" version >/dev/null 2>&1; then
      say "c0mpute already installed at $target (use --force to reinstall)"
      # Plugins and the TUI come from the installed binary's own release.
      if [ -n "$have_ver" ]; then RELEASE_TAG="v$have_ver"; fi
      return 0
    fi
    warn "existing $target appears broken (won't run); reinstalling"
  fi

  artifact="c0mpute-${platform}.tar.gz"
  url="${RELEASE_BASE}/${C0MPUTE_VERSION}/${artifact}"
  sig_url="${url}.minisig"
  tmp="$SCRATCH/c0mpute"
  mkdir -p "$tmp"

  say "downloading c0mpute ${C0MPUTE_VERSION}"
  http_code=$(curl -sSL -o "$tmp/$artifact" -w '%{http_code}' "$url" 2>/dev/null || echo "000")
  if [ "$http_code" != "200" ] && [ "$C0MPUTE_VERSION" = "latest" ]; then
    # The newest release can be public before its binaries finish uploading
    # (or a platform build failed). Walk back through recent releases and
    # take the first one that has this platform's asset.
    warn "latest c0mpute release has no ${artifact} yet (HTTP ${http_code}); trying the previous releases"
    for tag in $(recent_release_tags); do
      try_url="${RELEASE_BASE}/${tag}/${artifact}"
      code=$(curl -sSL -o "$tmp/$artifact" -w '%{http_code}' "$try_url" 2>/dev/null || echo "000")
      if [ "$code" = "200" ]; then
        say "using c0mpute ${tag}"
        url="$try_url"
        sig_url="${url}.minisig"
        http_code=200
        break
      fi
    done
  fi
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
  # Fail closed: every release artifact ships a minisign signature
  # (<artifact>.minisig, key in minisign.pub at the repo root). No signature,
  # no verifier, a bad signature or a signature for a different file all abort.
  sig_code=$(curl -sSL -o "$tmp/$artifact.minisig" -w '%{http_code}' "$sig_url" 2>/dev/null || echo "000")
  if [ "$sig_code" != "200" ]; then
    if [ "${C0MPUTE_ALLOW_UNSIGNED:-0}" = "1" ]; then
      warn "no signature at ${sig_url} (HTTP ${sig_code}); installing UNVERIFIED because C0MPUTE_ALLOW_UNSIGNED=1"
      case "$url" in */latest/*) ;; *) RELEASE_TAG="${url%/*}"; RELEASE_TAG="${RELEASE_TAG##*/}" ;; esac
    else
      rm -rf "$tmp"
      die "no signature at ${sig_url} (HTTP ${sig_code}); refusing to install an unverified binary (C0MPUTE_ALLOW_UNSIGNED=1 overrides)"
    fi
  else
    say "verifying signature for c0mpute"
    # A pinned release (or one found by walking back) must be signed for that
    # tag; through the `latest` URL any release's signature is accepted here
    # and its signed version is checked below.
    ms_tag=""
    case "$url" in */latest/*) ;; *) ms_tag="${url%/*}"; ms_tag="${ms_tag##*/}" ;; esac
    c0mpute_verify "$platform" "$tmp/$artifact" "$artifact" "$ms_tag"
    [ -n "$SIGNED_VERSION" ] || die "signature for ${artifact} names no release version; not installing it"
    ok "signature verified (minisign key $(printf '%s' "$C0MPUTE_PUBKEY" | cut -c1-12)…)"
    # "latest" is whatever the release host answers. The signature proves this
    # is a real release, not that it is the newest one, so `latest` never moves
    # an existing install backwards. Pin C0MPUTE_VERSION to downgrade on purpose.
    if [ "$C0MPUTE_VERSION" = "latest" ] && [ -n "$have_ver" ] && version_lt "$SIGNED_VERSION" "$have_ver"; then
      die "the release offered as latest is ${SIGNED_VERSION}, older than the installed c0mpute ${have_ver}; refusing to downgrade (to install it on purpose: C0MPUTE_VERSION=${SIGNED_VERSION})"
    fi
    RELEASE_TAG="$SIGNED_VERSION"
    say "installing c0mpute ${SIGNED_VERSION} (the version named in the signed release comment)"
  fi

  tar -xzf "$tmp/$artifact" -C "$C0MPUTE_HOME/bin"
  chmod +x "$C0MPUTE_HOME/bin/c0mpute"
  rm -rf "$tmp"
  ok "installed c0mpute → $C0MPUTE_HOME/bin/c0mpute"
  # git's remote helper for c0mpute:// URLs is this same binary under another
  # name. Relative, so it survives `c0mpute update` replacing the binary.
  # Without it: "git: 'remote-c0mpute' is not a git command".
  ln -sf c0mpute "$C0MPUTE_HOME/bin/git-remote-c0mpute" \
    && ok "git clone/push c0mpute:// ready (git-remote-c0mpute → c0mpute)"
}

# ────────────────────────────────────────────────────────────────────────
# Plugin installers (coinpay, infernet, transcode)
# ────────────────────────────────────────────────────────────────────────

# Unpack the release's signed c0mpute-plugins.tar.gz once. PLUGINS_STATE:
#   ok      PLUGINS_SRC holds the verified plugins/<id>/install.sh files
#   legacy  a release from before that archive: unverified URLs, with a warning
#   failed  it should be there and could not be had: run nothing unverified
PLUGINS_STATE=""
PLUGINS_SRC=""
fetch_plugins() {
  [ -z "$PLUGINS_STATE" ] || return 0
  PLUGINS_STATE=failed
  fp_rc=0
  fetch_signed_asset "$1" c0mpute-plugins.tar.gz || fp_rc=$?
  case "$fp_rc" in
    0)
      mkdir -p "$SCRATCH/plugins-src"
      if tar -xzf "$ASSET_PATH" -C "$SCRATCH/plugins-src" 2>/dev/null; then
        PLUGINS_SRC="$SCRATCH/plugins-src/plugins"
        PLUGINS_STATE=ok
      else
        warn "could not unpack c0mpute-plugins.tar.gz"
      fi
      ;;
    2) PLUGINS_STATE=legacy ;;
  esac
}

# Run plugin $1's installer: the copy in this checkout when run from one, else
# the copy in the signed archive, else (old releases only) the unsigned $2.
run_plugin_installer() {
  if [ -n "$REPO_ROOT" ] && [ -f "$REPO_ROOT/plugins/$1/install.sh" ]; then
    say "running $1 plugin installer (local: $REPO_ROOT/plugins/$1/install.sh)"
    sh "$REPO_ROOT/plugins/$1/install.sh" </dev/null
    return
  fi
  fetch_plugins "$platform"
  case "$PLUGINS_STATE" in
    ok)
      if [ ! -f "$PLUGINS_SRC/$1/install.sh" ]; then
        warn "the signed plugins archive has no installer for $1"
        return 1
      fi
      say "running $1 plugin installer (signed, c0mpute ${RELEASE_TAG:-latest})"
      sh "$PLUGINS_SRC/$1/install.sh" </dev/null
      ;;
    legacy)
      warn "c0mpute ${RELEASE_TAG:-(unknown release)} predates signed plugin installers; running $2 UNVERIFIED"
      curl -fsSL "$2" | sh
      ;;
    *)
      warn "no verified installer for $1; not running an unverified one"
      return 1
      ;;
  esac
}

chain_install() {
  name="$1"
  url="$2"

  # Always run the plugin installer so `curl | sh` upgrades plugins in place —
  # the coinpay/infernet installers are idempotent and update an existing
  # install. (Previously we skipped when already on PATH, so plugins went stale.)
  if command -v "$name" >/dev/null 2>&1; then
    say "upgrading $name"
  else
    say "installing $name"
  fi
  if ! run_plugin_installer "$name" "$url"; then
    warn "$name install/upgrade failed (continuing without it)"
    return 1
  fi
}

# House models: Infernet nodes pull and serve the models Infernet publishes
# (https://infernetprotocol.com/house-models.json) that fit the box. Persist any
# opt-out into infernet's config and say clearly what will happen.
house_models_notice() {
  command -v infernet >/dev/null 2>&1 || return 0
  if [ "${INFERNET_NO_HOUSE_MODELS:-}" = "1" ]; then
    infernet house-models off >/dev/null 2>&1 || true
    ok "house models: off (--no-house-models)"
    return 0
  fi
  if [ "${INFERNET_NO_UNCENSORED_MODELS:-}" = "1" ]; then
    infernet house-models uncensored off >/dev/null 2>&1 || true
  elif [ "${INFERNET_UNCENSORED_MODELS:-}" = "1" ]; then
    infernet house-models uncensored on >/dev/null 2>&1 || true
  fi
  say "house models: ON — this node pulls and serves the models listed at"
  say "  https://infernetprotocol.com/house-models.json that fit it (RAM; keeps 10% of disk free)."
  if [ "${INFERNET_UNCENSORED_MODELS:-}" = "1" ] && [ "${INFERNET_NO_UNCENSORED_MODELS:-}" != "1" ]; then
    say "  uncensored house models: ON (you opted in with --uncensored-models)"
  else
    say "  uncensored house models: off (opt in: infernet house-models uncensored on)"
  fi
  say "  opt out any time: infernet house-models off"
}

# transcode is in-process (no separate binary), but it depends on
# ffmpeg. Delegate to the plugin's own install.sh (see run_plugin_installer).
install_transcode_deps() {
  if ! run_plugin_installer transcode "$TRANSCODE_INSTALL_URL"; then
    warn "transcode deps install failed (continuing without ffmpeg)"
    return 1
  fi
}

# ────────────────────────────────────────────────────────────────────────
# desktop app (opt-in: --desktop)
# ────────────────────────────────────────────────────────────────────────
# The release carries a version-less AppImage per arch
# (c0mpute-desktop-linux-<arch>.AppImage, see .github/workflows/release.yml),
# signed like the binary (<asset>.minisig) and verified here. Only a release
# from before desktop signatures falls back to its .sha256, which comes from
# the same place as the file and so only catches a corrupt download. The app
# updates itself from GitHub releases afterwards. macOS and Windows builds are
# on the releases page.
install_desktop() {
  plat=$(detect_platform)
  case "$plat" in
    linux-*) ;;
    *)
      warn "desktop app: get the macOS build from https://github.com/profullstack/c0mpute/releases/latest"
      return 0
      ;;
  esac
  asset="c0mpute-desktop-${plat}.AppImage"
  bin_dir="$HOME/.local/bin"
  dest="$bin_dir/c0mpute-desktop"
  mkdir -p "$bin_dir" "$HOME/.local/share/applications"
  say "installing the desktop app ($asset)"
  rel="${RELEASE_TAG:-$C0MPUTE_VERSION}"
  base="${RELEASE_BASE}/${rel}"
  mkdir -p "$SCRATCH/assets"
  dl="$SCRATCH/assets/$asset"
  if ! curl -fsSL -o "$dl" "$base/$asset"; then
    rm -f "$dl"
    warn "desktop app: $asset is not on the ${rel} release (yet); skipped"
    return 0
  fi
  sig_code=$(curl -sSL -o "$dl.minisig" -w '%{http_code}' "$base/$asset.minisig" 2>/dev/null || echo "000")
  if [ "$sig_code" = "200" ]; then
    c0mpute_verify "$plat" "$dl" "$asset" "$RELEASE_TAG"
    ok "desktop app: signature verified (${SIGNED_VERSION})"
  elif [ "$sig_code" = "404" ] && legacy_release; then
    warn "desktop app: c0mpute ${rel} predates signed desktop builds; checking only the .sha256 from the same release"
    if ! curl -fsSL -o "$dl.sha256" "$base/$asset.sha256"; then
      rm -f "$dl" "$dl.sha256"
      warn "desktop app: no checksum for $asset on the ${rel} release; not installed"
      return 0
    fi
    want=$(cut -d' ' -f1 < "$dl.sha256")
    got=$(c0mpute_sha256 "$dl" || true)
    if [ -z "$want" ] || [ "$want" != "$got" ]; then
      rm -f "$dl" "$dl.sha256"
      warn "desktop app: checksum mismatch for $asset; not installed"
      return 0
    fi
  else
    # The desktop builds are signed a few minutes after a release goes public.
    rm -f "$dl" "$dl.minisig"
    warn "desktop app: no signature for $asset on the ${rel} release yet (HTTP ${sig_code}); not installed. Re-run with --desktop later."
    return 0
  fi
  chmod 755 "$dl" && mv -f "$dl" "$dest"
  chmod 755 "$dest.tmp" && mv "$dest.tmp" "$dest"
  cat > "$HOME/.local/share/applications/c0mpute-desktop.desktop" <<DESKTOP
[Desktop Entry]
Name=c0mpute
Comment=c0mpute chat, agent, node manager and hosting
Exec=$dest %U
Terminal=false
Type=Application
Categories=Network;
StartupWMClass=c0mpute-desktop
DESKTOP
  say "desktop app: $dest (menu entry: c0mpute)"
}

# ────────────────────────────────────────────────────────────────────────
# c0mpute-tui (Bun terminal UI on @profullstack/hqtui)
#
# `c0mpute tui` subprocess-launches a `c0mpute-tui` binary on PATH. We
# install the TUI as SOURCE run through Bun, wrapped by a small launcher —
# NOT a compiled binary. Source comes from the in-repo copy when running
# in-tree, otherwise the release's signed c0mpute-tui.tar.gz (verified like
# the binary). Its npm dependencies are still resolved by `bun install` at
# install time. Optional: a download failure warns and continues (the rest of
# c0mpute works without the TUI); a bad signature aborts.
# ────────────────────────────────────────────────────────────────────────
# Unverified: apps/tui out of the GitHub tarball of branch $1. Prints its path.
fetch_tui_branch() {
  say "fetching c0mpute-tui source" >&2
  mkdir -p "$SCRATCH/tui-branch"
  if curl -fsSL "https://github.com/profullstack/c0mpute/archive/refs/heads/$1.tar.gz" \
      | tar -xz -C "$SCRATCH/tui-branch" 2>/dev/null; then
    find "$SCRATCH/tui-branch" -type d -path '*/apps/tui' 2>/dev/null | head -1
  fi
}

install_tui() {
  bun_bin="$(command -v bun || true)"
  [ -x "$HOME/.bun/bin/bun" ] && bun_bin="${bun_bin:-$HOME/.bun/bin/bun}"
  if [ -z "$bun_bin" ]; then
    warn "bun not available; skipping c0mpute-tui (run: c0mpute tui after installing bun)"
    return 1
  fi

  wrapper="$C0MPUTE_HOME/bin/c0mpute-tui"
  # Only skip if the existing file is actually OUR bun launcher. Earlier
  # installers shipped a `bun build --compile` binary that crashed at runtime
  # (the old react-blessed TUI's dynamic requires couldn't be bundled); those
  # must be replaced even without --force. Detect our wrapper by its
  # `bun run` marker (a compiled binary won't match under grep).
  if [ -x "$wrapper" ] && [ "$FORCE" -eq 0 ] && grep -q 'bun run' "$wrapper" 2>/dev/null; then
    say "c0mpute-tui already installed at $wrapper (use --force to reinstall)"
    return 0
  fi
  if [ -e "$wrapper" ] && ! grep -q 'bun run' "$wrapper" 2>/dev/null; then
    warn "replacing stale c0mpute-tui (old compiled build crashes) with the source launcher"
  fi

  dest="$C0MPUTE_HOME/tui"

  # Locate the TUI source: in-repo copy first, else the signed release archive.
  src=""
  if [ -n "$REPO_ROOT" ] && [ -d "$REPO_ROOT/apps/tui/src" ]; then
    src="$REPO_ROOT/apps/tui"
  elif [ -n "${C0MPUTE_TUI_REF:-}" ]; then
    # An explicit branch is a developer override; nothing signs a branch.
    warn "C0MPUTE_TUI_REF=${C0MPUTE_TUI_REF}: installing c0mpute-tui UNVERIFIED from that branch"
    src=$(fetch_tui_branch "$C0MPUTE_TUI_REF")
  else
    tui_rc=0
    fetch_signed_asset "$platform" c0mpute-tui.tar.gz || tui_rc=$?
    case "$tui_rc" in
      0)
        mkdir -p "$SCRATCH/tui-src"
        if tar -xzf "$ASSET_PATH" -C "$SCRATCH/tui-src" 2>/dev/null; then src="$SCRATCH/tui-src/tui"; fi
        ;;
      2)
        warn "c0mpute ${RELEASE_TAG:-(unknown release)} predates the signed TUI archive; fetching c0mpute-tui source UNVERIFIED"
        src=$(fetch_tui_branch master)
        ;;
    esac
  fi
  if [ -z "$src" ] || [ ! -d "$src/src" ]; then
    warn "couldn't fetch c0mpute-tui source; skipping (run: c0mpute tui later to retry)"
    return 1
  fi

  say "installing c0mpute-tui → $dest"
  rm -rf "$dest"
  mkdir -p "$dest"
  cp -R "$src/src" "$src/package.json" "$src/tsconfig.json" "$dest/" 2>/dev/null

  # Materialise a self-contained node_modules (the monorepo hoists deps, so a
  # copied node_modules would be dangling symlinks).
  if ! ( cd "$dest" && "$bun_bin" install --production --no-save >/dev/null 2>&1 ); then
    warn "c0mpute-tui dependency install failed; skipping"
    return 1
  fi

  cat > "$wrapper" <<EOF
#!/usr/bin/env sh
# c0mpute-tui launcher: runs the hqtui-based TUI from source through Bun.
BUN="\$(command -v bun || echo "\$HOME/.bun/bin/bun")"
exec "\$BUN" run "$dest/src/index.ts" "\$@"
EOF
  chmod +x "$wrapper"
  ok "installed c0mpute-tui → $wrapper"
}

# ────────────────────────────────────────────────────────────────────────
# PATH + diagnostics
# ────────────────────────────────────────────────────────────────────────

# `c0mpute` is reached through PATH, never through a root-owned directory:
# ~/.c0mpute/bin holds files this user can rewrite, and a symlink to one in
# /usr/local/bin would put a user-writable program on root's PATH (anything
# that can write as this user would run as root the next time root, or sudo,
# typed `c0mpute`). So:
#   - as a normal user: PATH comes from the shell rc files (ensure_path) and,
#     when sudo is there, the /etc/profile.d hook below. That hook only names
#     $HOME, so for every account it points at that account's own directory.
#   - as root (cloud GPU images): root owns the binary, so the symlink is safe
#     and makes `c0mpute` work in the current shell at once.
# All best-effort.
link_system_bin() {
  target="$C0MPUTE_HOME/bin/c0mpute"
  [ -x "$target" ] || return 0  # nothing installed to link

  if [ "$(id -u 2>/dev/null || echo 1)" = "0" ]; then
    for sysbin in /usr/local/bin /usr/bin /opt/bin; do
      [ -d "$sysbin" ] || continue
      if ln -sf "$target" "$sysbin/c0mpute" 2>/dev/null; then
        ln -sf "$target" "$sysbin/git-remote-c0mpute" 2>/dev/null || true
        ok "symlinked $sysbin/c0mpute → $target"; break
      fi
    done
  else
    # Earlier installers made that symlink with sudo. Removing it needs root,
    # so say how instead of doing it behind a sudo prompt.
    for sysbin in /usr/local/bin /usr/bin /opt/bin; do
      for old in "$sysbin/c0mpute" "$sysbin/git-remote-c0mpute"; do
        [ -L "$old" ] || continue
        old_to=$(readlink "$old" 2>/dev/null || true)
        case "$old_to" in
          "$HOME"/*|/home/*|/Users/*)
            warn "$old is a symlink into a home directory ($old_to), left by an earlier c0mpute installer."
            warn "  It puts a user-writable program on root's PATH. Remove it:  sudo rm $old"
            ;;
        esac
      done
    done
  fi

  if [ -d /etc/profile.d ]; then
    line='export PATH="$HOME/.c0mpute/bin:$HOME/.local/bin:$PATH"'
    if printf '# Added by c0mpute installer\n%s\n' "$line" > /etc/profile.d/c0mpute.sh 2>/dev/null; then
      ok "wrote /etc/profile.d/c0mpute.sh"
    elif [ -n "$SUDO" ] && printf '# Added by c0mpute installer\n%s\n' "$line" \
        | $SUDO tee /etc/profile.d/c0mpute.sh >/dev/null 2>&1; then
      ok "wrote /etc/profile.d/c0mpute.sh (via sudo)"
    fi
  fi
  unset target sysbin line old old_to
}

ensure_path() {
  # The rc file of the shell you actually use must exist, or a fresh account
  # (zsh with no ~/.zshrc yet) would get no PATH line at all.
  path_rc=$(shell_rc_for "$(detect_shell)")
  case "$path_rc" in
    */config.fish) ;;
    *) [ -e "$path_rc" ] || touch "$path_rc" 2>/dev/null || true ;;
  esac

  # Bash / Zsh / sh-style profile. Each rc file gets the PATH line.
  # Idempotent — won't duplicate lines on re-run.
  for rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
    [ -f "$rc" ] || continue
    # Must be writable. An rc file created root-owned by a prior sudo install
    # (common on cloud GPU images) makes `>> "$rc"` fail with a raw shell-level
    # "cannot create ...: Permission denied" that 2>/dev/null can't suppress.
    # Pre-check and skip — link_system_bin() covers that case through the
    # /etc/profile.d hook, so this is best-effort.
    if [ ! -w "$rc" ]; then
      warn "$rc not writable (try: sudo chown \$USER:\$USER $rc) — skipping PATH append"
      continue
    fi

    if ! grep -q '\.c0mpute/bin' "$rc"; then
      {
        printf '\n# Added by c0mpute installer\n'
        printf 'export PATH="$HOME/.c0mpute/bin:$HOME/.local/bin:$PATH"\n'
      } >> "$rc"
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
  fi

  if grep -q '\.c0mpute/bin' "$path_rc" 2>/dev/null; then
    case "$path_rc" in
      */config.fish) ok "PATH: $path_rc has: fish_add_path -p \$HOME/.c0mpute/bin \$HOME/.local/bin" ;;
      *) ok "PATH: $path_rc has: export PATH=\"\$HOME/.c0mpute/bin:\$HOME/.local/bin:\$PATH\"" ;;
    esac
  else
    warn "could not add c0mpute to PATH in $path_rc; add this line yourself:"
    warn "  export PATH=\"\$HOME/.c0mpute/bin:\$HOME/.local/bin:\$PATH\""
  fi
}

print_versions() {
  echo
  if [ -x "$C0MPUTE_HOME/bin/c0mpute" ]; then
    printf 'c0mpute installed:  %s\n'  "$("$C0MPUTE_HOME/bin/c0mpute" version 2>/dev/null | tail -1)"
  fi
  if [ -x "$C0MPUTE_HOME/bin/c0mpute-tui" ]; then
    printf 'c0mpute-tui:        installed (run: c0mpute tui)\n'
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

# The worker starts with TTS + STT enabled. Python's venv support is a
# separate OS package on Debian/Ubuntu, so verify it before creating a
# systemd service that would otherwise restart-loop before advertising.
speech_python_ready() {
  command -v python3 >/dev/null 2>&1 || return 1
  probe=$(mktemp -d 2>/dev/null) || return 1
  ready=0
  if python3 -m venv "$probe/venv" >/dev/null 2>&1 \
     && "$probe/venv/bin/python" -m pip --version >/dev/null 2>&1; then
    ready=1
  fi
  rm -rf "$probe"
  [ "$ready" -eq 1 ]
}

ensure_speech_python() {
  [ "$(uname -s)" = "Linux" ] || return 0
  speech_python_ready && return 0

  if [ -z "$SUDO" ] && [ "$(id -u)" != "0" ]; then
    probe=$(mktemp -d 2>/dev/null) || return 1
    if python3 -m venv --without-pip "$probe/venv" >/dev/null 2>&1; then
      rm -rf "$probe"
      warn "system venv pip support is unavailable; c0mpute will bootstrap pip in the user's speech environment"
      return 0
    fi
    rm -rf "$probe"
    warn "Python venv support is unavailable and sudo is not configured"
    return 1
  fi

  if command -v apt-get >/dev/null 2>&1; then
    say "installing Python 3 venv support for the default TTS/STT roles"
    speech_venv_package="python3-venv"
    if command -v python3 >/dev/null 2>&1; then
      speech_venv_package=$(python3 -c 'import sys; print(f"python{sys.version_info.major}.{sys.version_info.minor}-venv")' 2>/dev/null || echo python3-venv)
    fi
    if [ -n "$SUDO" ]; then
      $SUDO apt-get update || return 1
      $SUDO apt-cache show "$speech_venv_package" >/dev/null 2>&1 || speech_venv_package="python3-venv"
      $SUDO apt-get install -y python3 "$speech_venv_package" || return 1
    else
      apt-get update || return 1
      apt-cache show "$speech_venv_package" >/dev/null 2>&1 || speech_venv_package="python3-venv"
      apt-get install -y python3 "$speech_venv_package" || return 1
    fi
    speech_python_ready && return 0
  fi

  warn "Python venv + pip are required for the default TTS/STT roles; install your distribution's Python venv package"
  return 1
}

# ────────────────────────────────────────────────────────────────────────
# Storage relocation: symlink the shard data dir onto the largest
# writable volume.
#
# c0mpute stores Reed-Solomon shards under ~/.local/share/c0mpute/shards
# by default. On hosts with a small root fs and a big mounted volume
# (RunPod /workspace, Vast.ai /workspace or /data, Lambda /lambda, bare
# metal /mnt/<whatever>) the shard dir fills the overlay fast.
#
# Same approach as infernet's install.sh: scan `df -P`, pick the
# writable mount with the most free space (excluding $HOME's mount and
# virtual/system fs), require ≥2x more free space than $HOME, then
# symlink ~/.local/share/c0mpute → <volume>/c0mpute. The Rust side
# uses the standard XDG path; the symlink redirects the actual writes.
#
# Opt out: NO_STORAGE_RELOCATE=1
# ────────────────────────────────────────────────────────────────────────

detect_storage_volume() {
  [ "${NO_STORAGE_RELOCATE:-0}" = "1" ] && return 0

  data_dir="$HOME/.local/share/c0mpute"

  # Don't relocate over an existing real directory with content; the
  # operator already started using it.
  if [ -d "$data_dir" ] && [ ! -L "$data_dir" ] && [ -n "$(ls -A "$data_dir" 2>/dev/null)" ]; then
    return 0
  fi

  home_mp="$(df -P "$HOME" 2>/dev/null | awk 'NR==2 {print $6}')"
  home_kb="$(df -P "$HOME" 2>/dev/null | awk 'NR==2 {print $4}')"
  [ -n "$home_mp" ] || return 0

  best_mp=""
  best_kb=0
  while read -r _fs _blocks _used _avail _capacity _mp; do
    case "$_fs" in
      tmpfs|devtmpfs|overlay|proc|sysfs|cgroup*|mqueue|securityfs|pstore|debugfs|tracefs|configfs|fusectl|none|squashfs|nsfs|hugetlbfs|binfmt_misc|autofs)
        continue ;;
    esac
    case "$_mp" in
      /|/proc|/proc/*|/sys|/sys/*|/dev|/dev/*|/run|/run/*|/boot|/boot/*|/etc|/etc/*|/usr|/usr/*|/var/lib/docker*|/snap|/snap/*|/tmp)
        continue ;;
    esac
    [ "$_mp" = "$home_mp" ] && continue
    [ -w "$_mp" ] || continue
    [ "${_avail:-0}" -lt 10485760 ] && continue   # ≥10 GB free
    if [ "$_avail" -gt "$best_kb" ]; then
      best_kb="$_avail"
      best_mp="$_mp"
    fi
  done <<EOF
$(df -P 2>/dev/null | tail -n +2)
EOF

  [ -z "$best_mp" ] && return 0

  # Only relocate if the volume has ≥2x more free space than $HOME
  # (guards against e.g. a 16 GB USB drive on a 200 GB-free desktop).
  if [ "$best_kb" -lt $(( ${home_kb:-0} * 2 )) ]; then
    return 0
  fi

  target="$best_mp/c0mpute"
  mkdir -p "$target" 2>/dev/null || { warn "cannot mkdir $target; skipping relocation"; return 0; }
  mkdir -p "$(dirname "$data_dir")" 2>/dev/null || true

  # If the data dir is missing, an empty real dir, or already a symlink,
  # replace it with a symlink pointing at the big volume.
  if [ -L "$data_dir" ]; then
    current="$(readlink "$data_dir" 2>/dev/null)"
    if [ "$current" = "$target" ]; then
      ok "shard storage already symlinked → $target"
      return 0
    fi
    rm -f "$data_dir" 2>/dev/null || true
  elif [ -d "$data_dir" ]; then
    rmdir "$data_dir" 2>/dev/null || true
  fi
  ln -sf "$target" "$data_dir" 2>/dev/null || {
    warn "ln -s $target $data_dir failed; using $data_dir on root fs"
    return 0
  }

  free_g=$(( best_kb / 1024 / 1024 ))
  home_g=$(( ${home_kb:-0} / 1024 / 1024 ))
  ok "shard storage → $target (${free_g}G free, vs \$HOME ${home_g}G)"
}

# ────────────────────────────────────────────────────────────────────────
# Auto-start the worker so a node self-onboards on install/upgrade. Default:
# a systemd *user* service under your own account (no root, nothing written
# outside $HOME), with lingering so it starts at boot. A root-managed system
# service only with --system-service, or when a previous install made one.
# Falls back to a background daemon. Idempotent: restarts onto the new binary.
# ────────────────────────────────────────────────────────────────────────
worker_unit() { # <WantedBy> [User= line]
  cat <<UNIT
[Unit]
Description=c0mpute worker
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
${2:-}
Environment=HOME=$HOME
Environment=PATH=$C0MPUTE_HOME/bin:$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin
ExecStart=$C0MPUTE_HOME/bin/c0mpute worker start
# always, not on-failure: a worker that exits "cleanly" (status 0, SIGPIPE)
# for any reason other than being told to stop must come back.
Restart=always
RestartSec=5
# 90: replaced by a newer worker start of this identity; 91: a duplicate that
# stood down. Restarting either would fight the worker that replaced it.
# SIGTERM: \`c0mpute worker stop\` (and systemctl stop) mean stop.
RestartPreventExitStatus=90 91 SIGTERM

[Install]
WantedBy=$1
UNIT
}

# One identity, one worker: a user unit next to the system unit would start a
# second worker with the same identity at every boot.
remove_user_service() {
  unit="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/c0mpute-worker.service"
  [ -f "$unit" ] || return 0
  systemctl --user disable --now c0mpute-worker >/dev/null 2>&1
  rm -f "$unit"
  systemctl --user daemon-reload >/dev/null 2>&1
  ok "removed the c0mpute-worker user service (the system service runs this worker now)"
}

start_system_service() {
  { [ -n "$SUDO" ] || [ "$(id -u)" = "0" ]; } || return 1
  remove_user_service
  tmp=$(mktemp)
  worker_unit multi-user.target "User=$(id -un)" > "$tmp"
  if $SUDO cp "$tmp" /etc/systemd/system/c0mpute-worker.service 2>/dev/null; then
    rm -f "$tmp"
    $SUDO systemctl daemon-reload 2>/dev/null
    $SUDO systemctl enable c0mpute-worker >/dev/null 2>&1
    if $SUDO systemctl restart c0mpute-worker 2>/dev/null; then
      ok "worker running as a systemd system service as $(id -un) (auto-starts on boot, restarts on crash)"
      return 0
    fi
  fi
  rm -f "$tmp"
  return 1
}

start_user_service() {
  [ "$(id -u)" = "0" ] && return 1
  systemctl --user show-environment >/dev/null 2>&1 || return 1
  unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  mkdir -p "$unit_dir" || return 1
  worker_unit default.target > "$unit_dir/c0mpute-worker.service" || return 1
  "$C0MPUTE_HOME/bin/c0mpute" worker stop >/dev/null 2>&1 # a daemon from an earlier install
  systemctl --user daemon-reload 2>/dev/null
  systemctl --user enable c0mpute-worker >/dev/null 2>&1
  systemctl --user restart c0mpute-worker 2>/dev/null || return 1
  ok "worker running as a systemd user service as $(id -un) (no root; restarts on crash)"
  # Lingering starts user services at boot, before anyone logs in. logind
  # lets a user enable it for themselves without sudo on most distros.
  if [ "$(loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null)" = "yes" ] \
     || loginctl enable-linger "$(id -un)" >/dev/null 2>&1; then
    ok "  starts at boot (systemd lingering on for $(id -un); undo: loginctl disable-linger)"
  else
    warn "  starts when you log in. To start at boot too: sudo loginctl enable-linger $(id -un)"
  fi
  return 0
}

start_worker() {
  [ "$NO_START" -eq 1 ] && return 0
  bin="$C0MPUTE_HOME/bin/c0mpute"
  [ -x "$bin" ] || return 0
  say "starting the c0mpute worker (auto-registers + serves its models)"

  if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    # Keep a system service an earlier install (or --system-service) set up,
    # so an upgrade never leaves two workers running.
    if [ -f /etc/systemd/system/c0mpute-worker.service ]; then
      start_system_service && return 0
      # Never start a second worker next to the system service.
      warn "a c0mpute-worker system service from an earlier install exists and needs sudo to restart:"
      warn "  sudo systemctl restart c0mpute-worker"
      warn "  (or switch to a user service: sudo systemctl disable --now c0mpute-worker &&"
      warn "   sudo rm /etc/systemd/system/c0mpute-worker.service, then re-run this installer)"
      return 0
    fi
    if [ "$SYSTEM_SERVICE" = "1" ]; then
      start_system_service && return 0
      warn "systemd system service setup failed (needs sudo); using a user service instead"
    fi
    start_user_service && return 0
    if [ "$(id -u)" = "0" ]; then
      start_system_service && return 0
    fi
  fi

  # Fallback: detached daemon. Restart onto the new binary on upgrade.
  "$bin" worker stop >/dev/null 2>&1
  if "$bin" worker start -d >/dev/null 2>&1; then
    ok "worker started (background daemon)"
  else
    warn "couldn't auto-start the worker — run: c0mpute worker start -d"
  fi
}

# Public handles the worker puts in its capability ad for c0mpute.com/nodes.
# Neither is a secret, verified, or a payout; an existing value is kept unless
# a new one is passed.
#   ref : who invited you. `curl -fsSL https://c0mpute.com/install.sh |
#         C0MPUTE_REF=<code> sh` (links from c0mpute.com/r/<code>) -> $C0MPUTE_HOME/ref
#   name: who runs this node (your nickname). `... | sh -s -- --name <handle>`
#         or C0MPUTE_NAME=<handle> -> $C0MPUTE_HOME/name; later `c0mpute name <handle>`.
save_handle() { # <value> <file> <label>
  code=$(printf '%s' "${1:-}" | tr '[:upper:]' '[:lower:]')
  [ -n "$code" ] || return 0
  case "$code" in
    *[!a-z0-9_-]*) warn "ignoring $3 '$code' (use a-z, 0-9, - or _)"; return 0 ;;
  esac
  if [ "${#code}" -gt 32 ]; then
    warn "ignoring $3 longer than 32 characters"
    return 0
  fi
  mkdir -p "$C0MPUTE_HOME" && printf '%s\n' "$code" > "$C0MPUTE_HOME/$2" \
    && ok "$3 '$code' saved (shown on https://c0mpute.com/nodes)"
}

save_ref() {
  save_handle "${C0MPUTE_REF:-}" ref "referral code"
  save_handle "${C0MPUTE_NAME:-}" name "node name"
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
  # Downloads land here and are verified here; gone when the script ends.
  SCRATCH=$(mktemp -d)
  trap 'rm -rf "$SCRATCH"' EXIT
  REPO_ROOT=$(repo_root)

  if [ "$INSTALL_C0MPUTE" -eq 1 ]; then install_c0mpute "$platform"; fi
  if [ "$INSTALL_COINPAY" -eq 1 ];  then chain_install coinpay  "$COINPAY_INSTALL_URL"; fi
  if [ "$INSTALL_INFERNET" -eq 1 ]; then
    # House-model opt-outs reach infernet's installer through the environment.
    export INFERNET_NO_HOUSE_MODELS="${INFERNET_NO_HOUSE_MODELS:-}"
    export INFERNET_NO_UNCENSORED_MODELS="${INFERNET_NO_UNCENSORED_MODELS:-}"
    export INFERNET_UNCENSORED_MODELS="${INFERNET_UNCENSORED_MODELS:-}"
    chain_install infernet "$INFERNET_INSTALL_URL" || true
    house_models_notice
  fi

  # Symlink the shard data dir onto the biggest writable volume so the
  # overlay/root fs doesn't fill up. Idempotent + opt-out via
  # NO_STORAGE_RELOCATE=1. Same approach as infernet's installer.
  detect_storage_volume || true

  # profile.d hook FIRST — works even when the shell rc files below are
  # root-owned/unwritable. Prevents `command not found`.
  link_system_bin
  ensure_path

  # transcode is built into the c0mpute binary, but its system dep
  # (ffmpeg) lives in the plugin's installer.
  # Optional: a missing ffmpeg only disables transcode jobs, so it must not
  # abort the install (under `set -e` a failing last command in an `if` body
  # exits the script, and the worker never started).
  if [ "$INSTALL_TRANSCODE" -eq 1 ]; then install_transcode_deps || true; fi

  # c0mpute-tui: Bun + hqtui terminal UI launched by `c0mpute tui`. bun (a
  # pinned, checksummed release) is only installed for it.
  if [ "$INSTALL_TUI" -eq 1 ]; then
    ensure_bun "$platform" || true
    install_tui || true
  fi

  # Desktop app (apps/desktop): opt-in, a node is usually headless.
  if [ "$INSTALL_DESKTOP" -eq 1 ]; then install_desktop || true; fi

  if [ "$WORKER_MODE" -eq 1 ]; then worker_checks; fi

  # Default worker roles include speech, so install the missing Debian/Ubuntu
  # venv package before start_worker enables the systemd service.
  if [ "$NO_START" -eq 0 ]; then
    ensure_speech_python || die "could not prepare Python venv support required by the default TTS/STT worker roles"
  fi

  print_versions
  run_doctor

  save_ref

  # Auto-start the worker so the node self-onboards (init + register + serve +
  # distribute) with no manual step. Opt out with --no-start.
  start_worker

  cat <<EOF
Your node is starting up automatically — it registers itself and serves its
models to the network (distributed inference load-balances across nodes that
have the model). Check it with:
  c0mpute worker status     # running? + which models are eligible to distribute
  c0mpute -v                # versions

Disk provider (on by default): every node stores shards and holds sites for the
network, and lists its disk on d1sks (https://d1sks.pwa.pit.world) with a signed
heartbeat, behind NAT or not. Cap it with [storage] cap_bytes, or opt out in
~/.config/c0mpute/config.toml:
  [storage] enabled = false        # no storage role, not listed (or --no-storage, C0MPUTE_STORAGE=0)
  [storage] list_on_d1sks = false  # store for the network, stay off d1sks (or C0MPUTE_D1SKS=0)
  [hosting] enabled = false        # hold no sites (or --no-hosting, C0MPUTE_HOSTING=0)
Private mode never lists.

Optional — claim this node under your account (for the dashboard + earnings;
NOT required to serve): c0mpute login && infernet pubkey link

Run the worker in the background:
  c0mpute worker start -d   # detach as a daemon (PID file + log under ~/.local/share/c0mpute)
  c0mpute worker start -a   # start + stream the log; press Ctrl-D to detach, worker keeps running
  c0mpute worker status     # is it running?
  c0mpute worker stop       # stop the daemon
  For a managed service, install the systemd unit:
    scripts/systemd/c0mpute-worker.service  (see its header for setup)

Networking — to be dialable by other nodes (required for a bootstrap seed,
recommended for a worker so it can receive jobs), open your libp2p p2p port.
Pin it with C0MPUTE_P2P_PORT=<port> (default is a random port); 46337 is the
convention. Open it at BOTH layers if your host has a cloud firewall:
  ufw:       sudo ufw allow 46337/tcp
  DO CLI:    doctl compute firewall add-rules <firewall-id> \\
               --inbound-rules "protocol:tcp,ports:46337,address:0.0.0.0/0,address:::/0"
  Railway:   no host firewall — add a TCP Proxy (Settings → Networking) or,
             per DIP-0010, run the seed on a droplet with a stable public IP.
Verify from outside the box: https://check-host.net/check-tcp?host=<ip>:46337

Your node on the public board (it appears within a few minutes):
  https://c0mpute.com/nodes   (run `c0mpute bench` once to get a score)
Invite others with your own link: https://c0mpute.com/r/<your-handle>

Docs: https://c0mpute.com/docs
EOF

  # Auto-activate the new shell so the user doesn't have to run
  # `source ~/.bashrc` or `exec $SHELL` manually.
  #
  # When the user runs `curl ... | sh`, our `sh` is a child of their
  # interactive shell. Children can't modify the parent's environment,
  # but we CAN exec a fresh interactive shell that reads the rc files
  # (and thus has the new PATH). Stdin needs to point at the user's
  # actual terminal (/dev/tty), since the curl pipe stole stdin.
  #
  # Skip auto-exec when:
  #   - PATH already contains ~/.c0mpute/bin (no reload needed)
  #   - stdout isn't a TTY (CI / piped to file)
  #   - /dev/tty isn't readable (no controlling terminal)
  #   - user passed --no-exec
  if printf '%s' "$PATH" | grep -q '\.c0mpute/bin'; then
    return 0
  fi
  if [ "${NO_EXEC:-0}" = "1" ] || [ "${SKIP_EXEC:-0}" = "1" ]; then
    return 0
  fi
  if [ ! -t 1 ] || [ ! -r /dev/tty ]; then
    user_shell=$(detect_shell)
    rc_hint=$(shell_rc_for "$user_shell")
    printf '\n\033[1;33m! No TTY available for auto-reload.\033[0m\n'
    if [ "$user_shell" = "fish" ]; then
      printf '  Run: \033[1;36msource %s\033[0m\n\n' "$rc_hint"
    else
      printf '  Run: \033[1;36m. %s\033[0m   (or \033[1;36mexec $SHELL\033[0m)\n\n' "$rc_hint"
    fi
    return 0
  fi

  user_shell=$(detect_shell)
  shell_bin="${SHELL:-/bin/bash}"
  rm -rf "$SCRATCH"  # exec never returns, so the EXIT trap would not run
  printf '\n\033[1;36m→\033[0m starting fresh %s shell with c0mpute on $PATH\n' "$user_shell"
  printf '  (type \033[1;36mexit\033[0m to return to your original shell)\n\n'
  case "$user_shell" in
    fish) exec "$shell_bin" -i </dev/tty ;;
    *)    exec "$shell_bin" -i </dev/tty ;;
  esac

  if [ "$DEVELOPER_MODE" -eq 1 ]; then
    echo
    say "developer mode: env"
    echo "  C0MPUTE_HOME=$C0MPUTE_HOME"
    echo "  C0MPUTE_VERSION=$C0MPUTE_VERSION"
    echo "  RELEASE_BASE=$RELEASE_BASE"
    echo "  COINPAY_INSTALL_URL=$COINPAY_INSTALL_URL"
    echo "  INFERNET_INSTALL_URL=$INFERNET_INSTALL_URL"
    echo "  TRANSCODE_INSTALL_URL=$TRANSCODE_INSTALL_URL"
    echo "  PLATFORM=$platform"
  fi
}

main "$@"
