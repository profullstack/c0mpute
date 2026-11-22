#!/usr/bin/env node
// Writes node/crates/c0mpute-cli/src/money_policies.json from the
// @profullstack/chat-plugins package the web app pins, so the Rust CLI holds
// the same read-only allowlists as the server gate. The web test
// apps/web/src/lib/chat/plugin-policy-cli.test.ts fails when the JSON drifts
// from the package; rerun this after bumping it:
//
//   node scripts/gen-money-policies.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const pkg = join(root, "apps/web/node_modules/@profullstack/chat-plugins/src/index.js");
const { POLICIES, FINANCE_CATALOG, policyFor } = await import(pathToFileURL(pkg).href);

// Same shape as cliPolicies() in plugin-policy-cli.test.ts.
const out = {};
for (const [slug, p] of Object.entries(POLICIES)) {
  // Hosts the policy answers for: the catalog URL's host and its www. form,
  // kept only when the package's own policyFor() agrees.
  const entry = FINANCE_CATALOG.find((e) => e.slug === slug);
  const host = entry ? new URL(entry.url).hostname : null;
  const hosts = host ? [host, `www.${host}`].filter((h) => policyFor(`https://${h}/mcp`)?.slug === slug) : [];
  out[slug] = { kind: p.kind, hosts, read_only: [...p.readOnly].sort(), paper_writes: [...(p.paperWrites ?? [])].sort() };
}
writeFileSync(join(root, "node/crates/c0mpute-cli/src/money_policies.json"), JSON.stringify(out, null, 2) + "\n");
console.log("wrote node/crates/c0mpute-cli/src/money_policies.json");
