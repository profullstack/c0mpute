// The Rust CLI (`c0mpute chat`) embeds the money-plugin allowlists as JSON
// (node/crates/c0mpute-cli/src/money_policies.json) to decide which held calls
// are money actions: those get the approval card and are never approved by
// --yes. This test fails when that JSON drifts from the package the server
// enforces; fix it with `node scripts/gen-money-policies.mjs`.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { FINANCE_CATALOG, POLICIES, policyFor } from "./plugin-policy";

const JSON_PATH = join(import.meta.dir, "../../../../../node/crates/c0mpute-cli/src/money_policies.json");

// Same shape as scripts/gen-money-policies.mjs writes.
function cliPolicies() {
  const out: Record<string, { kind: string; hosts: string[]; read_only: string[]; paper_writes: string[] }> = {};
  for (const [slug, p] of Object.entries(POLICIES)) {
    const entry = FINANCE_CATALOG.find((e) => e.slug === slug);
    const host = entry ? new URL(entry.url).hostname : null;
    const hosts = host ? [host, `www.${host}`].filter((h) => policyFor(`https://${h}/mcp`)?.slug === slug) : [];
    const paper = (p as { paperWrites?: Set<string> }).paperWrites;
    out[slug] = { kind: p.kind, hosts, read_only: [...p.readOnly].sort(), paper_writes: [...(paper ?? [])].sort() };
  }
  return out;
}

describe("CLI money policies", () => {
  test("the CLI's embedded allowlists match @profullstack/chat-plugins", () => {
    const embedded = JSON.parse(readFileSync(JSON_PATH, "utf8"));
    expect(embedded).toEqual(cliPolicies());
  });

  test("every money plugin has at least one host", () => {
    for (const p of Object.values(cliPolicies())) expect(p.hosts.length).toBeGreaterThan(0);
  });
});
