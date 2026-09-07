#!/usr/bin/env python3
"""Check that the JSON examples in docs/protocol/ match the implementation.

Every full envelope shown in `docs/protocol/*.md` is canonicalized here, in
Python, and compared byte-for-byte against what `c0mpute-envelope` produced
for the same record. That makes this two checks in one:

1. **The docs are not fiction.** A field renamed in Rust without updating
   the spec page fails here, on the commit that does it.

2. **Canonicalization is genuinely portable.** Python's
   `json.dumps(sort_keys=True, separators=(",", ":"))` is the naive
   implementation the spec claims to be compatible with (see
   docs/protocol/canonical-json.md). If the Rust canonicalizer ever drifts
   into something a second implementation would not reproduce, the two
   stop agreeing here.

The second is the one worth having. c0mpute's Phase 1 acceptance criterion
is that job, offer and receipt hashes reproduce *across implementations*,
and a claim like that is only worth what it is tested against.

Usage:
    scripts/check-protocol-docs.py            # runs cargo itself
    scripts/check-protocol-docs.py --vectors <file>
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import re
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DOCS_GLOB = os.path.join(REPO, "docs", "protocol", "*.md")
VECTOR_CMD = [
    "cargo", "test", "-q", "-p", "c0mpute-envelope",
    "--test", "vectors", "--", "--nocapture", "print_vectors",
]


def canonicalize(obj) -> str:
    """The canonical JSON of docs/protocol/canonical-json.md.

    Sorted keys, no whitespace, UTF-8 values. Floats are rejected before we
    get here, so the one genuinely hard part of RFC 8785 never applies.
    """
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def find_float(obj, path="$"):
    """Return the path of the first float, or None. Floats cannot be signed."""
    if isinstance(obj, float):
        return path
    if isinstance(obj, dict):
        for k, v in obj.items():
            if (hit := find_float(v, f"{path}.{k}")):
                return hit
    if isinstance(obj, list):
        for i, v in enumerate(obj):
            if (hit := find_float(v, f"{path}[{i}]")):
                return hit
    return None


def load_vectors(text: str) -> dict[str, str]:
    """The canonical envelopes the Rust implementation printed."""
    out = {}
    for line in text.splitlines():
        line = line.strip()
        if not line.startswith('{"payload"'):
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(obj, dict) and "type" in obj:
            out[obj["type"]] = line
    return out


def is_full_envelope(obj) -> bool:
    """Distinguish a complete signed envelope from an illustrative snippet.

    Pages legitimately show fragments (a `price` object on its own) and one
    field-shape skeleton with an empty payload. Neither has anything to
    compare against; only a populated, signed envelope does.
    """
    return (
        isinstance(obj, dict)
        and {"v", "type", "signer", "payload", "sig"} <= obj.keys()
        and obj["payload"] != {}
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--vectors", help="file of printed vectors; runs cargo if omitted")
    args = ap.parse_args()

    if args.vectors:
        vector_text = open(args.vectors, encoding="utf-8").read()
    else:
        print(f"$ {' '.join(VECTOR_CMD)}")
        result = subprocess.run(VECTOR_CMD, cwd=REPO, capture_output=True, text=True)
        if result.returncode != 0:
            print("could not generate vectors:\n" + result.stdout + result.stderr)
            return 2
        vector_text = result.stdout

    vectors = load_vectors(vector_text)
    if not vectors:
        print("no vectors found — did tests/vectors.rs stop printing?")
        return 2
    print(f"{len(vectors)} envelope types from c0mpute-envelope\n")

    checked = skipped = failed = 0

    for path in sorted(glob.glob(DOCS_GLOB)):
        name = os.path.basename(path)
        blocks = re.findall(r"```json\n(.*?)\n```", open(path, encoding="utf-8").read(), re.S)
        for block in blocks:
            try:
                obj = json.loads(block)
            except json.JSONDecodeError:
                skipped += 1  # an inline fragment, not a document
                continue
            if not is_full_envelope(obj):
                skipped += 1
                continue

            checked += 1
            type_id = obj["type"]

            if (where := find_float(obj)):
                print(f"FAIL {name}: {type_id} has a float at {where}, which cannot be signed")
                failed += 1
                continue

            if type_id not in vectors:
                print(f"FAIL {name}: {type_id} has no vector in tests/vectors.rs to check against")
                failed += 1
                continue

            mine, theirs = canonicalize(obj), vectors[type_id]
            if mine != theirs:
                print(f"FAIL {name}: {type_id} does not match the implementation")
                for i, (a, b) in enumerate(zip(mine, theirs)):
                    if a != b:
                        lo = max(0, i - 50)
                        print(f"      first difference at byte {i}")
                        print(f"      docs: …{mine[lo:i + 50]}…")
                        print(f"      impl: …{theirs[lo:i + 50]}…")
                        break
                else:
                    print(f"      identical prefix; lengths {len(mine)} vs {len(theirs)}")
                failed += 1
                continue

            print(f"ok   {name}: {type_id}")

    print(f"\n{checked} envelope examples checked, {skipped} fragments skipped, {failed} failed")
    if failed:
        print(
            "\nEither the docs are stale, or the wire format changed. If the format "
            "change was deliberate, the payload's version identifier has to move with "
            "it — see dips/v2.x/0025-signed-envelopes-and-native-identity.md."
        )
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
