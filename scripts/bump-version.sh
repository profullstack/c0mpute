#!/usr/bin/env sh
# Bump every version pin across the monorepo.
#
# Usage: scripts/bump-version.sh <new-version>
#
#   scripts/bump-version.sh 0.1.1
#   scripts/bump-version.sh 0.2.0
#
# Touches:
#   - root package.json
#   - apps/<app>/package.json (workspace packages)
#   - packages/<pkg>/package.json
#   - root Cargo.toml [workspace.package].version (Rust crates inherit
#     this via `version.workspace = true`)
#   - plugins/<id>/module.toml manifest versions
#
# Does NOT git-tag — run that yourself once happy:
#   git commit -am "bump v$NEW" && git tag v$NEW && git push --tags
set -eu

NEW="${1:-}"
if [ -z "$NEW" ]; then
  echo "usage: $0 <new-version>" >&2
  exit 2
fi

if ! printf '%s' "$NEW" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.+-]*)?$'; then
  echo "✗ '$NEW' doesn't look like a semver" >&2
  exit 2
fi

cd "$(dirname "$0")/.."

# JSON package.json files. Use python's pathlib so we don't need to
# shell-loop over filenames (dash doesn't support `read -d`).
python3 - "$NEW" <<'PY'
import json, pathlib, sys
new = sys.argv[1]
for p in pathlib.Path('.').rglob('package.json'):
    parts = set(p.parts)
    if {'node_modules', '.next', 'target'} & parts:
        continue
    try:
        d = json.loads(p.read_text())
    except Exception:
        continue
    if 'version' in d:
        d['version'] = new
        p.write_text(json.dumps(d, indent=2) + '\n')
        print(f'  updated {p}')
PY

# Cargo workspace
python3 - "$NEW" <<'PY'
import sys, re, pathlib
new = sys.argv[1]
p = pathlib.Path('Cargo.toml')
text = p.read_text()
text = re.sub(
    r'(\[workspace\.package\][^\[]*?version\s*=\s*")[^"]+(")',
    r'\g<1>' + new + r'\g<2>',
    text, count=1, flags=re.S,
)
p.write_text(text)
print('  updated Cargo.toml')
PY

# Release-feed fallback version. apps/web serves /releases/latest.json from
# this constant whenever the C0MPUTE_LATEST_VERSION env var is unset (the prod
# default), and that feed is what `c0mpute update` + the auto-upgrade poller
# read. Bump it in lockstep or a fresh release never gets advertised.
python3 - "$NEW" <<'PY'
import re, pathlib, sys
new = sys.argv[1]
p = pathlib.Path('apps/web/src/app/releases/latest.json/route.ts')
if p.exists():
    text = p.read_text()
    text2 = re.sub(
        r'(const FALLBACK_VERSION = ")[^"]+(")',
        r'\g<1>' + new + r'\g<2>',
        text, count=1,
    )
    if text2 != text:
        p.write_text(text2)
        print(f'  updated {p}')
    else:
        print(f'  ! FALLBACK_VERSION not found in {p} (feed may not advertise {new})')
else:
    print(f'  ! {p} missing (skipped feed fallback bump)')
PY

# Plugin manifests
for f in plugins/*/module.toml; do
  python3 -c "
import re, pathlib
p = pathlib.Path('$f')
text = p.read_text()
text = re.sub(r'(version\s*=\s*\")[^\"]+(\")', r'\g<1>$NEW\g<2>', text, count=1)
p.write_text(text)
print('  updated', '$f')
"
done

echo
echo "→ bumped to $NEW"
echo "next: git diff && git commit -am \"bump v$NEW\" && git tag v$NEW && git push origin master --tags"
