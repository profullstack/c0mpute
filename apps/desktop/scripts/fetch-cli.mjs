// Puts the c0mpute CLI (the Rust binary) at resources/cli/c0mpute[.exe], where
// electron-builder's extraResources picks it up.
//
//   node scripts/fetch-cli.mjs --from <path-to-binary>     copy a local build
//   node scripts/fetch-cli.mjs --tag v0.3.28 [--platform linux-x86_64]
//                                                          download a release asset
//
// Release assets are the same tarballs install.sh uses
// (c0mpute-<platform>.tar.gz, .zip on Windows), checked against their .sha256.
import { chmodSync, cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const opt = (k) => {
  const i = args.indexOf(k);
  return i >= 0 ? args[i + 1] : undefined;
};

function hostPlatform() {
  const os = { linux: "linux", darwin: "darwin", win32: "windows" }[process.platform];
  const arch = { x64: "x86_64", arm64: "aarch64" }[process.arch];
  if (!os || !arch) throw new Error(`unsupported host ${process.platform}/${process.arch}`);
  return `${os}-${arch}`;
}

const platform = opt("--platform") ?? hostPlatform();
const exe = platform.startsWith("windows") ? "c0mpute.exe" : "c0mpute";
const dest = join(root, "resources", "cli");
rmSync(dest, { recursive: true, force: true });
mkdirSync(dest, { recursive: true });

const from = opt("--from");
if (from) {
  cpSync(from, join(dest, exe));
} else {
  const tag = opt("--tag");
  if (!tag) throw new Error("give --from <binary> or --tag <vX.Y.Z>");
  const asset = `c0mpute-${platform}.${platform.startsWith("windows") ? "zip" : "tar.gz"}`;
  const base = `https://github.com/profullstack/c0mpute/releases/download/${tag}/`;
  const get = async (name) => {
    const r = await fetch(base + name, { redirect: "follow" });
    if (!r.ok) throw new Error(`GET ${base + name}: HTTP ${r.status}`);
    return Buffer.from(await r.arrayBuffer());
  };
  const body = await get(asset);
  const want = (await get(`${asset}.sha256`)).toString("utf8").trim().split(/\s+/)[0];
  const got = createHash("sha256").update(body).digest("hex");
  if (want !== got) throw new Error(`${asset}: sha256 ${got} does not match ${want}`);
  const work = mkdtempSync(join(tmpdir(), "c0cli-"));
  writeFileSync(join(work, asset), body);
  // On Windows runners `tar` on PATH is Git Bash's GNU tar, which reads
  // `C:\...` as a remote host ("Cannot connect to C: resolve failed") and
  // cannot open zips. Windows ships bsdtar as System32\tar.exe: it takes
  // drive-letter paths and unpacks .zip.
  const tar =
    process.platform === "win32"
      ? join(process.env.SystemRoot || "C:\\Windows", "System32", "tar.exe")
      : "tar";
  const r = asset.endsWith(".zip")
    ? spawnSync(tar, ["-xf", join(work, asset), "-C", work], { stdio: "inherit" })
    : spawnSync(tar, ["-xzf", join(work, asset), "-C", work], { stdio: "inherit" });
  if (r.status !== 0) throw new Error(`could not unpack ${asset}`);
  cpSync(join(work, exe), join(dest, exe));
  rmSync(work, { recursive: true, force: true });
}
if (!exe.endsWith(".exe")) chmodSync(join(dest, exe), 0o755);
console.log(`cli: ${join(dest, exe)} (${readFileSync(join(dest, exe)).length} bytes)`);
