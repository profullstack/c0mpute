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
#   --minimal       Install only c0mpute (skip coinpay + infernet + transcode deps)
#   --no-coinpay    Skip CoinPay CLI
#   --no-infernet   Skip Infernet CLI
#   --no-transcode  Skip transcode plugin's system deps (ffmpeg)
#   --no-tui        Skip the c0mpute-tui terminal UI
#   --worker        Add Docker / FFmpeg readiness checks
#   --developer     Verbose diagnostics
#   --force         Reinstall over existing
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

# Route plugin installs through c0mpute.com so each wrapper can do its
# own error handling; the wrapper at https://c0mpute.com/plugins/<id>/install.sh
# is the in-repo plugins/<id>/install.sh and chains to upstream itself.
COINPAY_INSTALL_URL="${COINPAY_INSTALL_URL:-https://c0mpute.com/plugins/coinpay/install.sh}"
INFERNET_INSTALL_URL="${INFERNET_INSTALL_URL:-https://c0mpute.com/plugins/infernet/install.sh}"
TRANSCODE_INSTALL_URL="${TRANSCODE_INSTALL_URL:-https://c0mpute.com/plugins/transcode/install.sh}"

INSTALL_C0MPUTE=1
INSTALL_COINPAY=1
INSTALL_INFERNET=1
INSTALL_TRANSCODE=1
INSTALL_TUI=1
WORKER_MODE=0
DEVELOPER_MODE=0
FORCE=0

say()  { printf '\033[1;36m→\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }
ok()   { printf '\033[1;32m✓\033[0m %s\n' "$*"; }

# Privilege escalation helper for system-wide steps (the /usr/local/bin
# symlink + /etc/profile.d hook). Empty when already root or when sudo isn't
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
    --worker)       WORKER_MODE=1 ;;
    --developer)    DEVELOPER_MODE=1 ;;
    --force)        FORCE=1 ;;
    --no-exec)      NO_EXEC=1 ;;
    --help|-h)
      sed -n '2,20p' "$0"
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

  # Always run the upstream installer so `curl | sh` upgrades plugins in place —
  # the coinpay/infernet installers are idempotent and update an existing
  # install. (Previously we skipped when already on PATH, so plugins went stale.)
  if command -v "$name" >/dev/null 2>&1; then
    say "upgrading $name (via $url)"
  else
    say "installing $name (via $url)"
  fi
  if ! curl -fsSL "$url" | sh; then
    warn "$name install/upgrade failed (continuing without it)"
    return 1
  fi
}

# transcode is in-process (no separate binary), but it depends on
# ffmpeg. Delegate to the plugin's own install.sh — local copy if
# we're running in-repo, otherwise the published one.
install_transcode_deps() {
  local_path="$(dirname "$0")/../plugins/transcode/install.sh"
  if [ -f "$local_path" ]; then
    say "running transcode plugin installer (local: $local_path)"
    sh "$local_path" || warn "transcode deps install reported a problem"
    return 0
  fi
  say "running transcode plugin installer (via $TRANSCODE_INSTALL_URL)"
  if ! curl -fsSL "$TRANSCODE_INSTALL_URL" | sh; then
    warn "transcode deps install failed (continuing without ffmpeg)"
    return 1
  fi
}

# ────────────────────────────────────────────────────────────────────────
# c0mpute-tui (Bun/blessed terminal UI)
#
# `c0mpute tui` subprocess-launches a `c0mpute-tui` binary on PATH. The TUI
# is a Bun + blessed app; blessed loads its widgets via computed
# `require('./widgets/'+name)`, which `bun build --compile` CANNOT bundle
# (the standalone binary dies with "Cannot find module './widgets/node'").
# So we install it as SOURCE run through Bun, wrapped by a small launcher —
# NOT a compiled binary. Source comes from the in-repo copy when running
# in-tree, otherwise the repo tarball on GitHub. Optional: any failure warns
# and continues (the rest of c0mpute works without the TUI).
# ────────────────────────────────────────────────────────────────────────
install_tui() {
  bun_bin="$(command -v bun || true)"
  [ -x "$HOME/.bun/bin/bun" ] && bun_bin="${bun_bin:-$HOME/.bun/bin/bun}"
  if [ -z "$bun_bin" ]; then
    warn "bun not available; skipping c0mpute-tui (run: c0mpute tui after installing bun)"
    return 1
  fi

  wrapper="$C0MPUTE_HOME/bin/c0mpute-tui"
  # Only skip if the existing file is actually OUR bun launcher. Earlier
  # installers shipped a `bun build --compile` binary that crashes at runtime
  # ("Cannot find module './widgets/node'" — blessed's dynamic requires can't
  # be bundled); those must be replaced even without --force. Detect our
  # wrapper by its `bun run` marker (a compiled binary won't match under grep).
  if [ -x "$wrapper" ] && [ "$FORCE" -eq 0 ] && grep -q 'bun run' "$wrapper" 2>/dev/null; then
    say "c0mpute-tui already installed at $wrapper (use --force to reinstall)"
    return 0
  fi
  if [ -e "$wrapper" ] && ! grep -q 'bun run' "$wrapper" 2>/dev/null; then
    warn "replacing stale c0mpute-tui (old compiled build crashes) with the source launcher"
  fi

  dest="$C0MPUTE_HOME/tui"

  # Locate the TUI source: in-repo copy first, else fetch the repo tarball.
  local_tui="$(dirname "$0")/../apps/tui"
  src=""
  cleanup_tmp=""
  if [ -d "$local_tui/src" ]; then
    src="$local_tui"
  else
    say "fetching c0mpute-tui source"
    tmp=$(mktemp -d)
    cleanup_tmp="$tmp"
    ref="${C0MPUTE_TUI_REF:-master}"
    tarball="https://github.com/profullstack/c0mpute/archive/refs/heads/${ref}.tar.gz"
    if curl -fsSL "$tarball" | tar -xz -C "$tmp" 2>/dev/null; then
      src=$(find "$tmp" -type d -path '*/apps/tui' 2>/dev/null | head -1)
    fi
    if [ -z "$src" ] || [ ! -d "$src/src" ]; then
      warn "couldn't fetch c0mpute-tui source; skipping (run: c0mpute tui later to retry)"
      [ -n "$cleanup_tmp" ] && rm -rf "$cleanup_tmp"
      return 1
    fi
  fi

  say "installing c0mpute-tui → $dest"
  rm -rf "$dest"
  mkdir -p "$dest"
  cp -R "$src/src" "$src/package.json" "$src/tsconfig.json" "$dest/" 2>/dev/null

  # Materialise a self-contained node_modules (the monorepo hoists deps, so a
  # copied node_modules would be dangling symlinks).
  if ! ( cd "$dest" && "$bun_bin" install --no-save >/dev/null 2>&1 ); then
    warn "c0mpute-tui dependency install failed; skipping"
    [ -n "$cleanup_tmp" ] && rm -rf "$cleanup_tmp"
    return 1
  fi

  cat > "$wrapper" <<EOF
#!/usr/bin/env sh
# c0mpute-tui launcher: runs the Bun/blessed TUI from source.
# (bun --compile can't bundle blessed's dynamic widget requires, so we run source.)
BUN="\$(command -v bun || echo "\$HOME/.bun/bin/bun")"
exec "\$BUN" run "$dest/src/index.tsx" "\$@"
EOF
  chmod +x "$wrapper"
  [ -n "$cleanup_tmp" ] && rm -rf "$cleanup_tmp"
  ok "installed c0mpute-tui → $wrapper"
}

# ────────────────────────────────────────────────────────────────────────
# PATH + diagnostics
# ────────────────────────────────────────────────────────────────────────

# Make `c0mpute` reachable no matter the shell config. Shell rc files
# (ensure_path) only help NEW shells and silently do nothing when they're
# root-owned (cloud GPU images) — that's how you get `c0mpute: command not
# found` right after a "successful" install. Mirror infernet's installer:
#   1. Symlink the binary into a system bin dir (already on every PATH), so it
#      works in the CURRENT shell immediately.
#   2. Write /etc/profile.d so future login shells also see ~/.c0mpute/bin —
#      which is where the peer binaries (coinpay, infernet) and c0mpute-tui
#      live; the single symlink only covers the main binary.
# Both try a direct write first, then $SUDO. All best-effort.
link_system_bin() {
  target="$C0MPUTE_HOME/bin/c0mpute"
  [ -x "$target" ] || return 0  # nothing installed to link

  linked=""
  for sysbin in /usr/local/bin /usr/bin /opt/bin; do
    [ -d "$sysbin" ] || continue
    if ln -sf "$target" "$sysbin/c0mpute" 2>/dev/null; then
      linked="$sysbin/c0mpute"; ok "symlinked $linked → $target"; break
    fi
    if [ -n "$SUDO" ] && $SUDO ln -sf "$target" "$sysbin/c0mpute" 2>/dev/null; then
      linked="$sysbin/c0mpute"; ok "symlinked $linked → $target (via sudo)"; break
    fi
  done
  [ -n "$linked" ] || warn "no writable system bin dir — relying on shell rc / profile.d for PATH"

  if [ -d /etc/profile.d ]; then
    line='export PATH="$HOME/.c0mpute/bin:$HOME/.local/bin:$PATH"'
    if printf '# Added by c0mpute installer\n%s\n' "$line" > /etc/profile.d/c0mpute.sh 2>/dev/null; then
      ok "wrote /etc/profile.d/c0mpute.sh"
    elif [ -n "$SUDO" ] && printf '# Added by c0mpute installer\n%s\n' "$line" \
        | $SUDO tee /etc/profile.d/c0mpute.sh >/dev/null 2>&1; then
      ok "wrote /etc/profile.d/c0mpute.sh (via sudo)"
    fi
  fi
  unset target linked sysbin line
}

ensure_path() {
  # Bash / Zsh / sh-style profile. Each rc file gets the PATH line +
  # the right `mise activate <shell>` invocation. Idempotent — won't
  # duplicate lines on re-run.
  for rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
    [ -f "$rc" ] || continue
    # Must be writable. An rc file created root-owned by a prior sudo install
    # (common on cloud GPU images) makes `>> "$rc"` fail with a raw shell-level
    # "cannot create ...: Permission denied" that 2>/dev/null can't suppress.
    # Pre-check and skip — link_system_bin() already put c0mpute on PATH via
    # the /usr/local/bin symlink + /etc/profile.d hook, so this is best-effort.
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

  # Symlink the shard data dir onto the biggest writable volume so the
  # overlay/root fs doesn't fill up. Idempotent + opt-out via
  # NO_STORAGE_RELOCATE=1. Same approach as infernet's installer.
  detect_storage_volume || true

  # System-wide PATH (symlink + profile.d) FIRST — works even when the shell
  # rc files below are root-owned/unwritable. Prevents `command not found`.
  link_system_bin
  ensure_path

  # transcode is built into the c0mpute binary, but its system dep
  # (ffmpeg) lives in the plugin's installer.
  if [ "$INSTALL_TRANSCODE" -eq 1 ]; then install_transcode_deps; fi

  # c0mpute-tui: Bun/blessed terminal UI launched by `c0mpute tui`.
  if [ "$INSTALL_TUI" -eq 1 ]; then install_tui || true; fi

  if [ "$WORKER_MODE" -eq 1 ]; then worker_checks; fi

  print_versions
  run_doctor

  cat <<EOF
Next steps:
  c0mpute login             # sign in to coinpay + infernet (ties this node to your accounts)
  c0mpute worker register   # registers the node + sets up your payable DID
  c0mpute doctor
  c0mpute worker start      # foreground; add --gpu to serve transcode/inference jobs
  c0mpute infernet setup    # (optional) also serve AI inference on infernet

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
