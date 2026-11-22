import assert from "node:assert/strict";
import { test } from "node:test";
import { ed25519, x25519 } from "@noble/curves/ed25519.js";
import { blake3 } from "@noble/hashes/blake3.js";
import { bytesToHex, concatBytes, randomBytes } from "@noble/hashes/utils.js";
import {
  aesGcmDecrypt,
  b64url,
  FILE_CHUNK_BYTES,
  fingerprint,
  fromB64url,
  HEADER_LEN,
  hkdf32,
  link,
  manifestBytes,
  MAGIC,
  openSealed,
  padme,
  planSealed,
  siteIdFor,
  streamObject,
  unwrapCustody,
  uploadSealed,
} from "../src/index.js";

const pattern = (n) => Uint8Array.from({ length: n }, (_, i) => (i * 7 + (i >> 8)) & 255);

function newIdentity() {
  const secret = randomBytes(32);
  return { secret, identity: `c0ps1.${b64url(x25519.getPublicKey(secret))}` };
}

async function sealAll(plan) {
  const out = [];
  for (const p of plan.objects) {
    const pieces = [];
    const record = await streamObject(p, (piece) => {
      pieces.push(piece);
    });
    out.push({ p, blob: concatBytes(...pieces), record });
  }
  return out;
}

/** Trial-decrypt the 16 slots like the owner's CLI does. */
async function unwrapSlot(blob, secret) {
  const streamId = blob.subarray(8, 24);
  const commit = blob.subarray(24, 56);
  const me = x25519.getPublicKey(secret);
  for (let i = 0; i < 16; i++) {
    const slot = blob.subarray(60 + i * 80, 60 + (i + 1) * 80);
    const eph = slot.subarray(0, 32);
    try {
      const ss = x25519.getSharedSecret(secret, eph);
      const kek = hkdf32(ss, concatBytes(eph, me), new TextEncoder().encode("c0mpute-private-v1 wrap"));
      return await aesGcmDecrypt(kek, new Uint8Array(12), concatBytes(MAGIC, streamId, commit), slot.subarray(32));
    } catch {}
  }
  return null;
}

test("padme matches the Rust buckets", () => {
  for (const n of [0, 1, 100, 4095, 4096]) assert.equal(padme(n), 4096);
  assert.equal(padme(4097), 4352);
  for (let n = 4097; n < 2 ** 34; n = Math.floor((n * 3) / 2) + 7) {
    const p = padme(n);
    assert.ok(p >= n && (p - n) / n <= 0.12, `${n} -> ${p}`);
  }
});

test("base64url round trips", () => {
  for (const n of [0, 1, 2, 3, 31, 32, 33]) {
    const b = randomBytes(n);
    assert.deepEqual(fromB64url(b64url(b)), b);
  }
});

test("a sealed file opens with its key and only its owner's slot", async () => {
  const me = newIdentity();
  const data = pattern(300_000);
  const plan = await planSealed(data, { identity: me.identity, fileName: "clip.mp4", contentType: "video/mp4" });
  assert.equal(plan.objects.length, 1);
  const [{ p, blob, record }] = await sealAll(plan);
  assert.equal(blob.length, p.blobSize);
  assert.ok(MAGIC.every((b, i) => blob[i] === b));
  // Nothing readable in the ciphertext.
  const hay = new TextDecoder("latin1").decode(blob);
  assert.ok(!hay.includes("clip.mp4") && !hay.includes(p.name));
  const opened = await openSealed(blob, plan.key, plan.siteId);
  assert.deepEqual(opened.content, data);
  assert.equal(opened.meta.file_name, "clip.mp4");
  assert.equal(opened.meta.kind, "file");
  // The owner's slot holds the DEK; a stranger's does not.
  assert.equal(b64url(await unwrapSlot(blob, me.secret)), plan.key);
  assert.equal(await unwrapSlot(blob, randomBytes(32)), null);
  // Custody: the owner recovers the signing seed; it matches the object key.
  const seed = await unwrapCustody(opened.meta.custody, opened.meta.owner, me.secret);
  assert.equal(bytesToHex(ed25519.getPublicKey(seed)), opened.meta.owner);
  await assert.rejects(unwrapCustody(opened.meta.custody, opened.meta.owner, randomBytes(32)));
  // The hosting record signs the canonical one-file manifest.
  const hashes = [];
  for (let at = 0; at < blob.length; at += FILE_CHUNK_BYTES) hashes.push(bytesToHex(blake3(blob.subarray(at, at + FILE_CHUNK_BYTES))));
  assert.equal(record.manifest, bytesToHex(blake3(manifestBytes(p.name, blob.length, hashes))));
  assert.equal(record.site_id, siteIdFor(record.owner, record.name));
  assert.equal(record.size, blob.length);
  const msg = new TextEncoder().encode(`c0mpute-hosting-v1\n${record.site_id}\n${record.name}\n${record.owner}\n${record.manifest}\n${record.seq}\n${record.size}\n`);
  assert.ok(ed25519.verify(record.sig, msg, record.owner));
});

test("tampering is detected", async () => {
  const me = newIdentity();
  const plan = await planSealed(pattern(100_000), { identity: me.identity });
  const [{ blob }] = await sealAll(plan);
  const bad = blob.slice();
  bad[HEADER_LEN + 10] ^= 1;
  await assert.rejects(openSealed(bad, plan.key));
  await assert.rejects(openSealed(blob, b64url(randomBytes(32))));
});

test("empty and chunk-boundary sizes", async () => {
  const me = newIdentity();
  for (const n of [0, 1, 65_536 - 200, 65_536, 4 * 65_536 + 3]) {
    const plan = await planSealed(pattern(n), { identity: me.identity });
    const [{ blob }] = await sealAll(plan);
    assert.deepEqual((await openSealed(blob, plan.key)).content, pattern(n), `size ${n}`);
  }
});

test("big files split into parts whose keys live only in the root", async () => {
  const me = newIdentity();
  const data = pattern(250_000);
  const plan = await planSealed(data, { identity: me.identity, partBytes: 100_000 });
  assert.equal(plan.objects.length, 4);
  const sealed = await sealAll(plan);
  const root = await openSealed(sealed[3].blob, plan.key, plan.siteId);
  assert.equal(root.meta.kind, "multipart");
  assert.equal(root.meta.parts.length, 3);
  const back = [];
  for (const [i, part] of root.meta.parts.entries()) {
    assert.equal(await unwrapSlot(sealed[i].blob, me.secret), null, "parts have no recipient slots");
    const o = await openSealed(sealed[i].blob, part.key, part.site_id);
    assert.equal(o.meta.kind, "part");
    back.push(o.content);
  }
  assert.deepEqual(concatBytes(...back), data);
});

test("fingerprint and link", () => {
  const me = newIdentity();
  assert.match(fingerprint(me.identity), /^([0-9a-f]{2}:){7}[0-9a-f]{2}$/);
  assert.equal(link("c0p1:0123456789abcdef0123", "KEY"), "https://gateway.c0mpute.com/p/0123456789abcdef0123/#k=KEY");
  assert.throws(() => link("nope", "k"));
});

test("uploadSealed: the servers see ciphertext and ids, never the key", async () => {
  const me = newIdentity();
  const seen = [];
  const staged = new Map();
  const fakeFetch = async (url, init = {}) => {
    const body = init.body;
    seen.push({ url, body: body instanceof Uint8Array ? null : body ?? null });
    const u = new URL(url);
    const ok = (j, status = 200) => new Response(JSON.stringify(j), { status, headers: { "content-type": "application/json" } });
    if (u.pathname === "/api/v1/storage/identity") return ok({ linked: true, identity: me.identity, ingest_url: "https://gw.test" });
    if (u.pathname === "/api/v1/storage/uploads") return ok({ upload_id: "up_1", ticket: "T", ingest_url: "https://gw.test" }, 201);
    if (u.pathname.startsWith("/api/v1/storage/uploads/")) return ok({ status: "replicating", copies: 1 });
    const m = /^\/storage\/v1\/sealed\/([0-9a-f]{20})(\/finish)?$/.exec(u.pathname);
    if (m && init.method === "PUT") {
      assert.equal(init.headers.authorization, "Bearer T");
      const cur = staged.get(m[1]) ?? new Uint8Array(0);
      assert.equal(Number(u.searchParams.get("offset")), cur.length);
      staged.set(m[1], concatBytes(cur, body));
      return ok({ received: cur.length + body.length });
    }
    if (m && m[2]) {
      const { record } = JSON.parse(body);
      assert.equal(record.size, staged.get(m[1]).length);
      return ok({ status: "published" });
    }
    return ok({ error: "unexpected" }, 404);
  };
  const file = new Blob([pattern(70_000)], { type: "video/webm" });
  const res = await uploadSealed(file, { token: "c0k_test", baseUrl: "https://c0.test", fetch: fakeFetch });
  assert.match(res.ref, /^c0p1:[0-9a-f]{20}$/);
  assert.equal(res.status, "replicating");
  for (const s of seen) assert.ok(!s.body || !String(s.body).includes(res.key), `key leaked to ${s.url}`);
  const blob = staged.get(res.siteId);
  const opened = await openSealed(blob, res.key, res.siteId);
  assert.deepEqual(opened.content, pattern(70_000));
  assert.equal(opened.meta.content_type, "video/webm");
});
