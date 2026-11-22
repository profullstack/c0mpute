#!/usr/bin/env bun
// Sign the first-party Expert manifests (apps/web/src/lib/experts/manifests/*.json)
// with the Profullstack publisher key, in place.
//
//   C0MPUTE_EXPERTS_SIGNING_KEY=<32-byte seed, base64url> bun scripts/sign-expert-manifests.ts
//   bun scripts/sign-expert-manifests.ts --gen      # print a fresh seed + public key, sign nothing
//
// The seed lives in the logicsrc vault (profullstack / c0mpute / prod,
// C0MPUTE_EXPERTS_SIGNING_KEY). The public key must be in FIRST_PARTY_PUBLISHERS
// (apps/web/src/lib/experts/catalog.ts); the catalog refuses anything else.
// Edit a manifest => bump its version => re-run this => commit.

import { generateKeyPairSync } from "node:crypto";
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { privateKeyObject, publicKeyString, signManifest, validateManifest, verifySignature } from "../apps/web/src/lib/experts/manifest";

const DIR = join(import.meta.dir, "../apps/web/src/lib/experts/manifests");

if (process.argv.includes("--gen")) {
  const { privateKey } = generateKeyPairSync("ed25519");
  const der = privateKey.export({ format: "der", type: "pkcs8" }) as Buffer;
  console.log(`C0MPUTE_EXPERTS_SIGNING_KEY=${der.subarray(der.length - 32).toString("base64url")}`);
  console.log(`public key: ${publicKeyString(privateKey)}`);
  process.exit(0);
}

const secret = process.env.C0MPUTE_EXPERTS_SIGNING_KEY;
if (!secret) {
  console.error("C0MPUTE_EXPERTS_SIGNING_KEY is not set (logicsrc teams pull profullstack c0mpute prod)");
  process.exit(1);
}
const key = privateKeyObject(secret);
const pub = publicKeyString(key);
let bad = 0;
for (const f of readdirSync(DIR).filter((f) => f.endsWith(".json")).sort()) {
  const path = join(DIR, f);
  const { signature: _old, ...m } = JSON.parse(readFileSync(path, "utf8"));
  m.publisher = { ...m.publisher, key: pub };
  const signed = signManifest(m, key);
  const problems = validateManifest(signed);
  if (problems.length || !verifySignature(signed)) {
    console.error(`${f}: ${problems.join("; ") || "signature did not verify"}`);
    bad++;
    continue;
  }
  writeFileSync(path, `${JSON.stringify(signed, null, 2)}\n`);
  console.log(`signed ${f} (${signed.id}@${signed.version})`);
}
console.log(`publisher key ${pub}`);
process.exit(bad ? 1 : 0);
