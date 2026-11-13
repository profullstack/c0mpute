#!/usr/bin/env sh
# Build the two platform-independent release archives that scripts/install.sh
# and the CLI install from, so they can be signed like the binaries:
#
#   c0mpute-plugins.tar.gz   plugins/<id>/install.sh + plugins/<id>/module.toml
#   c0mpute-tui.tar.gz       tui/src, tui/package.json, tui/tsconfig.json
#
# Usage: scripts/pack-release-extras.sh <repo-root> <out-dir>
#
# Called by .github/workflows/release.yml (job `extras`) on the tagged tree;
# run it by hand to see exactly what a release ships. Writes <archive> and
# <archive>.sha256 into <out-dir>. Needs GNU tar (ubuntu runners, any Linux).
set -eu

root="${1:?usage: pack-release-extras.sh <repo-root> <out-dir>}"
out="${2:?usage: pack-release-extras.sh <repo-root> <out-dir>}"
[ -d "$root/plugins" ] || { echo "no plugins/ under $root" >&2; exit 1; }
[ -f "$root/apps/tui/src/index.ts" ] || { echo "no apps/tui/src/index.ts under $root" >&2; exit 1; }
[ -f "$root/apps/tui/package.json" ] || { echo "no apps/tui/package.json under $root" >&2; exit 1; }
mkdir -p "$out"
out=$(cd "$out" && pwd)
root=$(cd "$root" && pwd)

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

# Same bytes for the same tree: fixed order, owner and mtime, no gzip header time.
pack() { # <archive> <top-level dir inside $stage>
  tar --sort=name --owner=0 --group=0 --numeric-owner --mtime='2020-01-01 00:00:00Z' \
    -C "$stage" -cf - "$2" | gzip -n -9 > "$out/$1"
  ( cd "$out" && sha256sum "$1" > "$1.sha256" )
}

# Plugins: every installer, plus its manifest. The installers are
# self-contained (they read nothing else from the repo at install time).
mkdir -p "$stage/plugins"
n=0
for dir in "$root"/plugins/*/; do
  id=$(basename "$dir")
  [ -f "$dir/install.sh" ] || continue
  mkdir -p "$stage/plugins/$id"
  cp "$dir/install.sh" "$stage/plugins/$id/install.sh"
  chmod 755 "$stage/plugins/$id/install.sh"
  if [ -f "$dir/module.toml" ]; then
    cp "$dir/module.toml" "$stage/plugins/$id/module.toml"
    chmod 644 "$stage/plugins/$id/module.toml"
  fi
  n=$((n + 1))
done
[ "$n" -gt 0 ] || { echo "no plugins/*/install.sh under $root" >&2; exit 1; }
chmod -R u=rwX,go=rX "$stage/plugins"
# The installers scripts/install.sh runs by default must be there.
for id in coinpay infernet transcode; do
  [ -f "$stage/plugins/$id/install.sh" ] || { echo "plugins/$id/install.sh missing" >&2; exit 1; }
done
pack c0mpute-plugins.tar.gz plugins

# TUI: exactly what the installers copy into ~/.c0mpute/tui (never node_modules).
mkdir -p "$stage/tui"
cp -R "$root/apps/tui/src" "$stage/tui/src"
cp "$root/apps/tui/package.json" "$stage/tui/package.json"
[ -f "$root/apps/tui/tsconfig.json" ] && cp "$root/apps/tui/tsconfig.json" "$stage/tui/tsconfig.json"
chmod -R u=rwX,go=rX "$stage/tui"
pack c0mpute-tui.tar.gz tui

echo "packed $n plugin installer(s) and the TUI into $out:"
ls -l "$out/c0mpute-plugins.tar.gz" "$out/c0mpute-tui.tar.gz"
