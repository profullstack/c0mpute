#!/usr/bin/env node
// Seal a deterministic file for a storage identity and write what an app
// would upload: <site_id>.blob, <site_id>.record.json per object, and
// plan.json. The Rust interop test (node/crates/c0mpute-core/tests/
// sealed_js_interop.rs) opens them with `c0mpute storage get`'s code path.
//
//   node make-fixture.mjs <c0ps1...> <out dir> <size> [part bytes]
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { planSealed, streamObject } from "../src/index.js";

const [identity, out, sizeArg, partArg] = process.argv.slice(2);
if (!identity || !out || !sizeArg) {
  console.error("usage: make-fixture.mjs <c0ps1...> <out dir> <size> [part bytes]");
  process.exit(2);
}
const size = Number(sizeArg);
const data = Uint8Array.from({ length: size }, (_, i) => (i * 7 + (i >> 8)) & 255);
mkdirSync(out, { recursive: true });
const plan = await planSealed(data, {
  identity,
  fileName: "fixture.bin",
  contentType: "application/octet-stream",
  ...(partArg ? { partBytes: Number(partArg) } : {}),
});
for (const p of plan.objects) {
  const pieces = [];
  const record = await streamObject(p, (piece) => {
    pieces.push(piece);
  });
  writeFileSync(join(out, `${p.siteId}.blob`), Buffer.concat(pieces));
  writeFileSync(join(out, `${p.siteId}.record.json`), JSON.stringify(record));
}
writeFileSync(join(out, "plan.json"), JSON.stringify({ ref: plan.ref, site_id: plan.siteId, key: plan.key, site_ids: plan.objects.map((o) => o.siteId), size }));
console.log(plan.ref);
