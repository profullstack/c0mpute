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
const FALLBACK_VERSION = "0.3.37";
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

/**
 * The newest *published* GitHub release, cached for two minutes.
 *
 * Without an env override this feed used to announce FALLBACK_VERSION, which
 * goes live when the version-bump PR merges and the site redeploys, minutes
 * before the release workflow has built and published that tag's binaries.
 * Every node polling in that gap fetched a 404 for the archive or its .sha256
 * ("auto-upgrade: poll failed", 2026-11-09, v0.2.92/v0.2.93). The release
 * workflow publishes a release (draft until then) only once every platform
 * uploaded, so GitHub's latest release always has its assets.
 */
const RELEASE_CACHE_MS = 120_000;
let cachedRelease: { version: string; at: number } | null = null;

async function publishedVersion(): Promise<string | null> {
  const now = Date.now();
  if (cachedRelease && now - cachedRelease.at < RELEASE_CACHE_MS) return cachedRelease.version;
  // github.com/<repo>/releases/latest redirects to .../releases/tag/<tag>.
  // It is not the REST API, so it does not spend the 60/hour unauthenticated
  // quota that every process on the box shares (2026-11-09: quota at 0 kept
  // this feed on 0.3.13 while v0.3.15 was out, stalling every node's update).
  try {
    const r = await fetch("https://github.com/profullstack/c0mpute/releases/latest", {
      method: "HEAD",
      redirect: "manual",
      cache: "no-store",
      signal: AbortSignal.timeout(3_000),
    });
    const tag = (r.headers.get("location") ?? "").match(/\/releases\/tag\/(v?\d+\.\d+\.\d+)$/)?.[1];
    if (tag) {
      cachedRelease = { version: tag.replace(/^v/, ""), at: now };
      return cachedRelease.version;
    }
  } catch {
    // Fall through to the API.
  }
  try {
    const headers: Record<string, string> = { accept: "application/vnd.github+json" };
    if (process.env.GITHUB_TOKEN) headers.authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
    const r = await fetch("https://api.github.com/repos/profullstack/c0mpute/releases/latest", {
      headers,
      cache: "no-store",
      signal: AbortSignal.timeout(3_000),
    });
    if (r.ok) {
      const tag = ((await r.json()) as { tag_name?: unknown }).tag_name;
      if (typeof tag === "string" && /^v?\d+\.\d+\.\d+$/.test(tag)) {
        cachedRelease = { version: tag.replace(/^v/, ""), at: now };
        return cachedRelease.version;
      }
    }
  } catch {
    // Fall through: keep serving the last good answer.
  }
  // Rate-limited or down: the last published version we saw (retried after
  // the cache period), else the build-time fallback.
  if (cachedRelease) cachedRelease = { ...cachedRelease, at: now };
  return cachedRelease?.version ?? null;
}

async function loadManifest(): Promise<ReleaseManifest> {
  const channelRaw = process.env.C0MPUTE_RELEASE_CHANNEL;
  return {
    version:
      process.env.C0MPUTE_LATEST_VERSION || (await publishedVersion()) || FALLBACK_VERSION,
    channel: isChannel(channelRaw) ? channelRaw : "stable",
    min_required: process.env.C0MPUTE_MIN_REQUIRED || FALLBACK_MIN_REQUIRED,
    artifacts: loadArtifacts(),
    blocked_rollback: loadBlockedRollback(),
  };
}

export async function GET() {
  return NextResponse.json(await loadManifest(), {
    headers: { "cache-control": "public, max-age=60, s-maxage=60" },
  });
}
