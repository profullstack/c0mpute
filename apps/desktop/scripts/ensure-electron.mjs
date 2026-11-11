// bun does not run dependency lifecycle scripts unless the root package.json
// trusts them, and trusting electron at the root would make the c0mpute.com
// Docker build download a 100 MB Electron it never runs. So the desktop app
// fetches its Electron binary itself, only when it is actually needed.
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";

const require = createRequire(import.meta.url);
const pkg = require.resolve("electron/package.json");
const dir = dirname(pkg);
if (!existsSync(join(dir, "path.txt"))) {
  const r = spawnSync(process.execPath, [join(dir, "install.js")], { stdio: "inherit" });
  if (r.status !== 0) process.exit(r.status ?? 1);
}
