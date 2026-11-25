#!/usr/bin/env bash
# Sign the seeding manifest: the list of torrents c0mpute nodes may seed.
#
# FOR THE LIST PUBLISHER ONLY. Nodes never sign; they verify what this makes
# against the release key pinned in the binary (minisign.pub). See
# docs/seeding.md for the whole procedure and the legal rule.
#
#   scripts/sign-seeding-manifest.sh path/to/manifest.json
#
# The key is the c0mpute RELEASE key (the one release.yml signs binaries
# with; source of truth: logicsrc team vault c0mpute--signing). Give it as:
#
#   MINISIGN_SECRET_KEY_FILE=/path/to/minisign.key   (a file), or
#   MINISIGN_SECRET_KEY='untrusted comment: ...'     (the key text)
#   MINISIGN_PASSWORD=...                            (its password; asked
#                                                     for when unset)
#
# Writes <manifest>.minisig next to the manifest, with the trusted comment
# `timestamp:<now> file:seeding-manifest.json` (nodes refuse a signature that
# does not name that file, so a binary's signature cannot pass for a
# manifest), checks it with minisign against minisign.pub, and, when a
# `c0mpute` binary is on PATH, runs `c0mpute seeding verify` on it too.
#
# Never commit a secret key. This script reads it and deletes its temp copy.
set -euo pipefail

manifest="${1:-}"
if [ -z "$manifest" ] || [ ! -f "$manifest" ]; then
  echo "usage: $0 <manifest.json>  (see docs/seeding.md)" >&2
  exit 2
fi

repo="$(cd "$(dirname "$0")/.." && pwd)"
pub="$repo/minisign.pub"
[ -s "$pub" ] || { echo "minisign.pub not found at $pub" >&2; exit 1; }
command -v minisign >/dev/null || { echo "minisign is not installed (https://jedisct1.github.io/minisign/)" >&2; exit 1; }

# The checks a node makes before it reads an entry, so a mistake shows up
# here and not as a refused manifest on every node.
if command -v python3 >/dev/null; then
  python3 - "$manifest" <<'PY'
import json, re, sys
m = json.load(open(sys.argv[1]))
problems = []
if m.get("kind") != "c0mpute.seeding.manifest": problems.append('kind must be "c0mpute.seeding.manifest"')
if m.get("v") != 1: problems.append("v must be 1")
if not isinstance(m.get("seq"), int) or m["seq"] < 1: problems.append("seq must be a positive integer, higher than the last published one")
if not isinstance(m.get("issued_at"), int): problems.append("issued_at must be unix seconds")
seen = set()
for i, t in enumerate(m.get("torrents", [])):
    h = str(t.get("info_hash", "")).lower()
    if not re.fullmatch(r"[0-9a-f]{40}", h): problems.append(f"torrents[{i}]: info_hash is not 40 hex")
    if h in seen: problems.append(f"torrents[{i}]: {h} listed twice")
    seen.add(h)
    if not str(t.get("license", "")).strip(): problems.append(f"torrents[{i}]: license is required")
    if not str(t.get("source", "")).startswith("https://"): problems.append(f"torrents[{i}]: source must be an https URL")
    if not isinstance(t.get("bytes"), int) or t["bytes"] <= 0: problems.append(f"torrents[{i}]: bytes must be > 0")
if problems:
    print("manifest problems:\n  " + "\n  ".join(problems), file=sys.stderr)
    sys.exit(1)
print(f"manifest #{m['seq']}: {len(m.get('torrents', []))} torrent(s)")
PY
fi

umask 077
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
if [ -n "${MINISIGN_SECRET_KEY_FILE:-}" ]; then
  key="$MINISIGN_SECRET_KEY_FILE"
elif [ -n "${MINISIGN_SECRET_KEY:-}" ]; then
  key="$tmp/minisign.key"
  printf '%s\n' "$MINISIGN_SECRET_KEY" > "$key"
else
  echo "set MINISIGN_SECRET_KEY_FILE or MINISIGN_SECRET_KEY (the release key; see the header)" >&2
  exit 1
fi

comment="timestamp:$(date +%s) file:seeding-manifest.json"
if [ -n "${MINISIGN_PASSWORD:-}" ]; then
  printf '%s\n' "$MINISIGN_PASSWORD" | minisign -S -s "$key" -m "$manifest" -x "$manifest.minisig" -t "$comment" >/dev/null
else
  minisign -S -s "$key" -m "$manifest" -x "$manifest.minisig" -t "$comment"
fi

minisign -V -p "$pub" -m "$manifest" -x "$manifest.minisig"
if command -v c0mpute >/dev/null && c0mpute seeding --help >/dev/null 2>&1; then
  c0mpute seeding verify "$manifest" --sig "$manifest.minisig"
fi
echo "signed: $manifest.minisig"
echo "publish both files so that <manifest_url> and <manifest_url>.minisig serve them (docs/seeding.md)."
