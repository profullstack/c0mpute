// Bundles main, preload and renderer with esbuild into dist/. Every runtime
// dependency (MCP SDK, TOML, YAML, unzip) is bundled into the main process
// file, so the packaged app has no node_modules and electron-builder never has
// to understand bun's hoisted workspace layout.
import { build } from "esbuild";
import { cpSync, mkdirSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const out = join(root, "dist");
rmSync(out, { recursive: true, force: true });
mkdirSync(join(out, "renderer"), { recursive: true });

const common = { bundle: true, sourcemap: "linked", logLevel: "warning", target: "node22" };

await build({
  ...common,
  entryPoints: [join(root, "src/main/index.ts")],
  outfile: join(out, "main/index.cjs"),
  platform: "node",
  format: "cjs",
  external: ["electron"],
});

await build({
  ...common,
  entryPoints: [join(root, "src/preload/index.ts")],
  outfile: join(out, "preload/index.cjs"),
  platform: "node",
  format: "cjs",
  external: ["electron"],
});

await build({
  ...common,
  entryPoints: [join(root, "src/renderer/app.ts")],
  outfile: join(out, "renderer/app.js"),
  platform: "browser",
  format: "iife",
  target: "chrome130",
});

cpSync(join(root, "src/renderer/index.html"), join(out, "renderer/index.html"));
cpSync(join(root, "src/renderer/styles.css"), join(out, "renderer/styles.css"));
cpSync(join(root, "resources/icon.png"), join(out, "renderer/icon.png"));
console.log("built", out);
