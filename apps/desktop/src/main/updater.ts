// App updates from GitHub releases (the same `v*` releases the CLI ships in).
//
// An AppImage is replaced in place (download next to it, chmod, rename) and
// the user is asked to restart. Other installs (.deb, .dmg, .exe) get a
// notice with the release link: their package manager or installer owns the
// files. C0MPUTE_NO_AUTO_UPDATE=1 turns it off (dev binaries, tests).

import { chmodSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

export const RELEASES_API = "https://api.github.com/repos/profullstack/c0mpute/releases/latest";

/** Numeric x.y.z comparison (0.10.0 > 0.9.0). Pre-release tags are older. */
export function isNewer(candidate: string, current: string): boolean {
  const parse = (v: string) => v.replace(/^v/, "").split("-")[0].split(".").map((n) => Number.parseInt(n, 10) || 0);
  const a = parse(candidate);
  const b = parse(current);
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    if ((a[i] ?? 0) !== (b[i] ?? 0)) return (a[i] ?? 0) > (b[i] ?? 0);
  }
  return false;
}

export interface Release {
  tag_name: string;
  html_url: string;
  draft?: boolean;
  prerelease?: boolean;
  assets: { name: string; browser_download_url: string }[];
}

/** The desktop asset for this platform, by electron-builder's artifactName. */
export function pickAsset(rel: Release, platform: NodeJS.Platform, arch: string, kind: "AppImage" | "deb" | "dmg" | "exe"): Release["assets"][number] | undefined {
  const a = arch === "x64" ? ["x86_64", "x64", "amd64"] : arch === "arm64" ? ["arm64", "aarch64"] : [arch];
  return rel.assets.find((x) => x.name.startsWith("c0mpute-desktop-") && x.name.endsWith(`.${kind}`) && a.some((k) => x.name.includes(k)));
}

export async function latestRelease(version: string): Promise<Release | null> {
  try {
    const r = await fetch(RELEASES_API, {
      headers: { accept: "application/vnd.github+json", "user-agent": `c0mpute-desktop/${version}` },
      signal: AbortSignal.timeout(10_000),
    });
    if (!r.ok) return null;
    const rel = (await r.json()) as Release;
    return rel.draft || rel.prerelease ? null : rel;
  } catch {
    return null;
  }
}

export type UpdateResult =
  | { kind: "none" }
  | { kind: "disabled" }
  | { kind: "installed"; version: string }
  | { kind: "available"; version: string; url: string };

export async function checkForUpdate(opts: { version: string; appImage?: string; env?: NodeJS.ProcessEnv }): Promise<UpdateResult> {
  const env = opts.env ?? process.env;
  if (env.C0MPUTE_NO_AUTO_UPDATE === "1" || env.C0MPUTE_NO_AUTO_UPDATE === "true") return { kind: "disabled" };
  const rel = await latestRelease(opts.version);
  if (!rel || !isNewer(rel.tag_name, opts.version)) return { kind: "none" };
  const version = rel.tag_name.replace(/^v/, "");
  if (opts.appImage) {
    const asset = pickAsset(rel, process.platform, process.arch, "AppImage");
    if (asset) {
      const r = await fetch(asset.browser_download_url, { signal: AbortSignal.timeout(600_000) });
      if (r.ok) {
        const tmp = join(dirname(opts.appImage), `.c0mpute-update-${process.pid}.AppImage`);
        try {
          writeFileSync(tmp, Buffer.from(await r.arrayBuffer()));
          chmodSync(tmp, 0o755);
          renameSync(tmp, opts.appImage);
          return { kind: "installed", version };
        } catch {
          try {
            unlinkSync(tmp);
          } catch {}
        }
      }
    }
  }
  return { kind: "available", version, url: rel.html_url };
}
