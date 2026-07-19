/**
 * Serves the release manifest at https://c0mpute.com/releases/latest.json.
 *
 * This is the feed polled by `c0mpute update`/`upgrade` (the update crate's
 * `DEFAULT_RELEASE_FEED`) and by the auto-upgrade poll loop. Without this route
 * the CLI's upgrade check 404s and errors out, and the poll loop logs a failure
 * on every tick.
 *
 * The manifest is env-var driven — mirroring `bootstrap.json` — so a new release
 * can be published (version bumped, artifacts added) by editing env vars, with
 * no code deploy. Shape must match `node/crates/c0mpute-update/src/lib.rs`:
 *
 *   { version, channel, min_required, artifacts[], blocked_rollback[] }
 *   artifact = { os, arch, url, sha256_hex, minisig_url }
 *
 * A missing/malformed env config degrades to a valid manifest pinned at the
 * fallback version below — a 404/500 here would break the upgrade flow network
 * -wide, so this route always returns a well-formed 200.
 */

import { NextResponse } from "next/server";

// Read env per-request so releases publish without a rebuild.
export const dynamic = "force-dynamic";

/**
 * Fallback "latest" version, used when `C0MPUTE_LATEST_VERSION` is unset. Keep
 * this in sync with the workspace version in `Cargo.toml` on each release so a
 * freshly built CLI reports "already latest" rather than a phantom upgrade.
 */
const FALLBACK_VERSION = "0.2.24";
const FALLBACK_MIN_REQUIRED = "0.0.1";

type Channel = "stable" | "beta" | "nightly";

interface Artifact {
  os: string; // "linux" | "darwin" | "windows"
  arch: string; // "x86_64" | "aarch64"
  url: string;
  sha256_hex: string;
  minisig_url: string;
}

interface ReleaseManifest {
  version: string;
  channel: Channel;
  min_required: string;
  artifacts: Artifact[];
  blocked_rollback: string[];
}

function isChannel(v: unknown): v is Channel {
  return v === "stable" || v === "beta" || v === "nightly";
}

function loadArtifacts(): Artifact[] {
  const raw = process.env.C0MPUTE_RELEASE_ARTIFACTS_JSON;
  if (!raw) return [];
  try {
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    // Drop any entry missing a required string field — a bad artifact must not
    // poison the whole manifest (the CLI iterates artifacts to find its match).
    return parsed.filter(
      (a): a is Artifact =>
        a &&
        typeof a.os === "string" &&
        typeof a.arch === "string" &&
        typeof a.url === "string" &&
        typeof a.sha256_hex === "string" &&
        typeof a.minisig_url === "string",
    );
  } catch {
    return [];
  }
}

function loadBlockedRollback(): string[] {
  const raw = process.env.C0MPUTE_BLOCKED_ROLLBACK_JSON;
  if (!raw) return [];
  try {
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((v): v is string => typeof v === "string");
  } catch {
    return [];
  }
}

function loadManifest(): ReleaseManifest {
  const channelRaw = process.env.C0MPUTE_RELEASE_CHANNEL;
  return {
    version: process.env.C0MPUTE_LATEST_VERSION || FALLBACK_VERSION,
    channel: isChannel(channelRaw) ? channelRaw : "stable",
    min_required: process.env.C0MPUTE_MIN_REQUIRED || FALLBACK_MIN_REQUIRED,
    artifacts: loadArtifacts(),
    blocked_rollback: loadBlockedRollback(),
  };
}

export async function GET() {
  return NextResponse.json(loadManifest(), {
    headers: { "cache-control": "public, max-age=60, s-maxage=60" },
  });
}
