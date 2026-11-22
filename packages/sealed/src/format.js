// The c0mpute-private-v1 sealed object format, in JavaScript.
//
// Byte-for-byte the construction of the Rust crate `c0mpute-sealed`
// (node/crates/c0mpute-sealed/src/format.rs), which is the reference: an
// object made here opens with `c0mpute storage get`, and CI checks it does.
// Nothing here invents crypto: X25519, Ed25519, HKDF-SHA256, AES-256-GCM in
// the STREAM construction, BLAKE3, Padme padding, exactly as there.
//
// AES-GCM runs on WebCrypto (browsers, Node 20+, Deno, Bun; React Native with
// a WebCrypto polyfill such as react-native-quick-crypto). The curves and
// hashes are @noble (pure JS, audited), so they work wherever JS does.

import { ed25519, x25519 } from "@noble/curves/ed25519.js";
import { blake3 } from "@noble/hashes/blake3.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { sha256 } from "@noble/hashes/sha2.js";
import { bytesToHex, concatBytes, randomBytes, utf8ToBytes } from "@noble/hashes/utils.js";

export const MAGIC = new Uint8Array([0x43, 0x30, 0x50, 0x52, 0x49, 0x56, 0x00, 0x01]); // "C0PRIV\0\1"
export const MAX_RECIPIENTS = 16;
const SLOT_LEN = 32 + 32 + 16;
const FIXED_LEN = 8 + 16 + 32 + 4;
const SLOTS_LEN = MAX_RECIPIENTS * SLOT_LEN;
const ROSTER_LEN = 12 + (1 + MAX_RECIPIENTS * 32) + 16;
/** Fixed header size (1881 bytes). */
export const HEADER_LEN = FIXED_LEN + SLOTS_LEN + ROSTER_LEN;
/** Plaintext bytes per AEAD chunk. */
export const CHUNK_PLAINTEXT = 64 * 1024;
export const TAG_LEN = 16;
const CHUNK_CIPHERTEXT = CHUNK_PLAINTEXT + TAG_LEN;
const SIG_LEN = 64;
/** Content per part: bigger files are split (the Rust PART_BYTES). */
export const PART_BYTES = 192 * 1024 * 1024;
/** The hosting layer cuts a site file into chunks of this size. */
export const FILE_CHUNK_BYTES = 1024 * 1024;
/** Every sealed stream is at least this long. */
export const MIN_PADDED = 4096;
export const IDENTITY_PREFIX = "c0ps1.";

const te = new TextEncoder();
const INFO_COMMIT = te.encode("c0mpute-private-v1 commit");
const INFO_BODY = te.encode("c0mpute-private-v1 body");
const INFO_WRAP = te.encode("c0mpute-private-v1 wrap");
const INFO_CUSTODY = te.encode("c0mpute-private-v1 custody");
const SIG_DOMAIN = "c0mpute-private-v1 object";
const RECORD_DOMAIN = "c0mpute-hosting-v1";

function subtle() {
  const s = globalThis.crypto?.subtle;
  if (!s) throw new Error("WebCrypto (crypto.subtle) is required: a browser, Node 20+, or a polyfill on React Native");
  return s;
}

export const hkdf32 = (ikm, salt, info) => hkdf(sha256, ikm, salt, info, 32);

// ── base64url ────────────────────────────────────────────────────────────────
const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
export function b64url(bytes) {
  let out = "";
  let i = 0;
  for (; i + 2 < bytes.length; i += 3) {
    const n = (bytes[i] << 16) | (bytes[i + 1] << 8) | bytes[i + 2];
    out += B64[(n >> 18) & 63] + B64[(n >> 12) & 63] + B64[(n >> 6) & 63] + B64[n & 63];
  }
  if (i < bytes.length) {
    const n = (bytes[i] << 16) | ((bytes[i + 1] ?? 0) << 8);
    out += B64[(n >> 18) & 63] + B64[(n >> 12) & 63];
    if (i + 1 < bytes.length) out += B64[(n >> 6) & 63];
  }
  return out;
}

export function fromB64url(s) {
  const clean = s.trim().replace(/=+$/, "");
  if (!/^[A-Za-z0-9_-]*$/.test(clean)) throw new Error("not base64url");
  const out = new Uint8Array(Math.floor((clean.length * 3) / 4));
  let bits = 0;
  let acc = 0;
  let j = 0;
  for (const ch of clean) {
    acc = (acc << 6) | B64.indexOf(ch);
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out[j++] = (acc >> bits) & 0xff;
    }
  }
  return out.subarray(0, j);
}

// ── identities ───────────────────────────────────────────────────────────────
/** The 32-byte X25519 public key of a `c0ps1.<base64url>` storage identity. */
export function parseIdentity(identity) {
  if (typeof identity !== "string" || !identity.trim().startsWith(IDENTITY_PREFIX)) {
    throw new Error("not a storage identity (expected c0ps1...)");
  }
  const key = fromB64url(identity.trim().slice(IDENTITY_PREFIX.length));
  if (key.length !== 32 || key.every((b) => b === 0)) throw new Error("storage identity must be a 32-byte non-zero key");
  return key;
}

/** `c0mpute storage key show`'s fingerprint: first 8 bytes of BLAKE3, colon-separated. */
export function fingerprint(identity) {
  const h = blake3(parseIdentity(identity));
  return Array.from(h.subarray(0, 8), (b) => b.toString(16).padStart(2, "0")).join(":");
}

// ── small helpers ────────────────────────────────────────────────────────────
function u64be(n) {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, BigInt(n));
  return b;
}

function chunkNonce(i, last) {
  const n = new Uint8Array(12);
  n.set(u64be(i), 3);
  n[11] = last ? 1 : 0;
  return n;
}

function chunkAad(streamId, commit, i, last) {
  return concatBytes(MAGIC, streamId, commit, u64be(i), new Uint8Array([last ? 1 : 0]));
}

const slotAad = (streamId, commit) => concatBytes(MAGIC, streamId, commit);

async function aesKey(raw) {
  return subtle().importKey("raw", raw, { name: "AES-GCM" }, false, ["encrypt", "decrypt"]);
}

export async function aesGcmEncrypt(key, nonce, aad, data) {
  const k = key instanceof Uint8Array ? await aesKey(key) : key;
  return new Uint8Array(await subtle().encrypt({ name: "AES-GCM", iv: nonce, additionalData: aad, tagLength: 128 }, k, data));
}

export async function aesGcmDecrypt(key, nonce, aad, data) {
  const k = key instanceof Uint8Array ? await aesKey(key) : key;
  return new Uint8Array(await subtle().decrypt({ name: "AES-GCM", iv: nonce, additionalData: aad, tagLength: 128 }, k, data));
}

/** Padme bucket of `len` (never below 4096): pad.rs. */
export function padme(len) {
  if (len <= MIN_PADDED) return MIN_PADDED;
  const e = len.toString(2).length - 1; // floor(log2 len)
  const s = e.toString(2).length; // floor(log2 e) + 1
  const lastBits = e - s;
  const step = 2 ** lastBits;
  return Math.ceil(len / step) * step;
}

/** Hosting site id for an owner key (hex) and name: 20 hex chars of BLAKE3. */
export function siteIdFor(ownerHex, name) {
  return bytesToHex(blake3(utf8ToBytes(`c0mpute-site\n${ownerHex}\n${name}`))).slice(0, 20);
}

/** 128 random bits as 26 lowercase base32 chars (a valid hosting site name). */
export function randomName() {
  const ALPHA = "abcdefghijklmnopqrstuvwxyz234567";
  const b = randomBytes(16);
  let out = "";
  let acc = 0;
  let bits = 0;
  for (const byte of b) {
    acc = ((acc << 8) | byte) & 0xffff;
    bits += 8;
    while (bits >= 5) {
      bits -= 5;
      out += ALPHA[(acc >> bits) & 31];
    }
  }
  if (bits > 0) out += ALPHA[(acc << (5 - bits)) & 31];
  return out;
}

function sharedSecret(secret, pub) {
  const ss = x25519.getSharedSecret(secret, pub);
  if (ss.every((b) => b === 0)) throw new Error("recipient key is a low-order point");
  return ss;
}

/** The per-object owner key (hex) for a 32-byte Ed25519 signing seed. */
export const ownerHex = (seed) => bytesToHex(ed25519.getPublicKey(seed));

/**
 * Wrap a per-object signing seed to the owner's storage key (Meta.custody in
 * the Rust crate): e_pub || AES-256-GCM(HKDF(X25519(e, R), e_pub || R,
 * "c0mpute-private-v1 custody"), 0^96, magic || owner key, seed).
 */
export async function wrapCustody(seed, recipientKey) {
  const owner = ed25519.getPublicKey(seed);
  const eph = randomBytes(32);
  const ephPub = x25519.getPublicKey(eph);
  const kek = hkdf32(sharedSecret(eph, recipientKey), concatBytes(ephPub, recipientKey), INFO_CUSTODY);
  const ct = await aesGcmEncrypt(kek, new Uint8Array(12), concatBytes(MAGIC, owner), seed);
  return b64url(concatBytes(ephPub, ct));
}

/** Inverse of wrapCustody, for the holder of the X25519 secret (tests, tools). */
export async function unwrapCustody(custody, ownerHexKey, x25519Secret) {
  const blob = fromB64url(custody);
  if (blob.length !== 80) throw new Error("custody has the wrong length");
  const eph = blob.subarray(0, 32);
  const me = x25519.getPublicKey(x25519Secret);
  const kek = hkdf32(sharedSecret(x25519Secret, eph), concatBytes(eph, me), INFO_CUSTODY);
  const owner = hexToBytes(ownerHexKey);
  const seed = await aesGcmDecrypt(kek, new Uint8Array(12), concatBytes(MAGIC, owner), blob.subarray(32));
  if (bytesToHex(ed25519.getPublicKey(seed)) !== ownerHexKey) throw new Error("custody seed does not match the object key");
  return seed;
}

export function hexToBytes(hex) {
  if (!/^([0-9a-f]{2})*$/i.test(hex)) throw new Error("not hex");
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

async function buildSlots(dek, streamId, commit, recipients) {
  if (recipients.length > MAX_RECIPIENTS) throw new Error(`at most ${MAX_RECIPIENTS} recipients`);
  const aad = slotAad(streamId, commit);
  const slots = [];
  for (const r of recipients) {
    const eph = randomBytes(32);
    const ephPub = x25519.getPublicKey(eph);
    const kek = hkdf32(sharedSecret(eph, r), concatBytes(ephPub, r), INFO_WRAP);
    slots.push(concatBytes(ephPub, await aesGcmEncrypt(kek, new Uint8Array(12), aad, dek)));
  }
  while (slots.length < MAX_RECIPIENTS) {
    // A real ephemeral public key and random bytes: shaped like a real slot.
    slots.push(concatBytes(x25519.getPublicKey(randomBytes(32)), randomBytes(48)));
  }
  for (let i = slots.length - 1; i > 0; i--) {
    const j = new DataView(randomBytes(4).buffer).getUint32(0) % (i + 1);
    [slots[i], slots[j]] = [slots[j], slots[i]];
  }
  return concatBytes(...slots);
}

/**
 * One sealed object, planned: keys, name, site id, metadata and exact size
 * are fixed up front, so an upload can be sized before a byte is encrypted.
 * `content(start, end)` reads the object's plaintext (async, Uint8Array).
 */
export function planObject({ kind, size, content, recipients, owner, fileName, contentType, parts, created, version }) {
  const seed = randomBytes(32);
  const dek = randomBytes(32);
  const streamId = randomBytes(16);
  const name = randomName();
  const objOwner = ownerHex(seed);
  const siteId = siteIdFor(objOwner, name);
  return {
    kind,
    size,
    content,
    recipients,
    owner,
    fileName,
    contentType,
    parts: parts ?? [],
    created,
    version,
    seed,
    dek,
    streamId,
    name,
    objOwner,
    siteId,
    custody: null,
    metaBytes: null,
  };
}

/** Fill in custody + metadata (needs async WebCrypto); then sizes are exact. */
export async function finalizePlan(p) {
  p.custody = await wrapCustody(p.seed, p.owner);
  const meta = {
    v: 1,
    kind: p.kind,
    name: p.name,
    site_id: p.siteId,
    owner: p.objOwner,
    size: p.size,
    ...(p.fileName ? { file_name: p.fileName } : {}),
    ...(p.contentType ? { content_type: p.contentType } : {}),
    created: p.created,
    version: p.version,
    ...(p.parts.length ? { parts: p.parts } : {}),
    custody: p.custody,
  };
  p.metaBytes = utf8ToBytes(JSON.stringify(meta));
  const unpadded = 4 + p.metaBytes.length + SIG_LEN + p.size;
  p.padded = padme(unpadded);
  p.chunks = Math.ceil(p.padded / CHUNK_PLAINTEXT);
  p.blobSize = HEADER_LEN + p.padded + p.chunks * TAG_LEN;
  return p;
}

const READ_SLICE = 4 * 1024 * 1024;

/** Pass 1: BLAKE3 of the plaintext content (needed for the owner signature). */
async function hashContent(p, onRead) {
  const h = blake3.create({});
  for (let at = 0; at < p.size; at += READ_SLICE) {
    const end = Math.min(p.size, at + READ_SLICE);
    h.update(await p.content(at, end));
    onRead?.(end - at);
  }
  return h.digest();
}

/**
 * Pass 2: the ciphertext, as an async iterable of Uint8Array pieces (the
 * header, then one sealed 64 KiB chunk at a time). Reads the content twice
 * (hash, then encrypt) and holds at most a few MiB at once.
 */
export async function* ciphertext(p, { onRead } = {}) {
  if (!p.metaBytes) await finalizePlan(p);
  const contentHash = await hashContent(p, onRead);
  const commit = hkdf32(p.dek, p.streamId, INFO_COMMIT);
  const msg = utf8ToBytes(
    `${SIG_DOMAIN}\n${p.siteId}\n${bytesToHex(p.streamId)}\n${bytesToHex(commit)}\n${bytesToHex(blake3(p.metaBytes))}\n${bytesToHex(contentHash)}`,
  );
  const sig = ed25519.sign(msg, p.seed);
  const slots = await buildSlots(p.dek, p.streamId, commit, p.recipients);
  // The owner-only roster needs a key derived from the owner's secret, which
  // an app never has: random bytes of the right shape. The owner's CLI treats
  // an object with custody as wrapped to the owner alone.
  const roster = randomBytes(ROSTER_LEN);
  const count = new Uint8Array([0, MAX_RECIPIENTS, 0, 0]);
  yield concatBytes(MAGIC, p.streamId, commit, count, slots, roster);

  const bodyKey = await aesKey(hkdf32(p.dek, p.streamId, INFO_BODY));
  const metaLen = new Uint8Array(4);
  new DataView(metaLen.buffer).setUint32(0, p.metaBytes.length);
  const prefix = concatBytes(metaLen, p.metaBytes, sig);

  // Plaintext stream = prefix | content | zero padding, cut into 64 KiB chunks.
  const total = p.padded;
  let buf = new Uint8Array(0);
  let produced = 0; // plaintext bytes emitted into chunks so far
  let index = 0;
  const emit = async (chunk) => {
    const last = index === p.chunks - 1;
    const ct = await aesGcmEncrypt(bodyKey, chunkNonce(index, last), chunkAad(p.streamId, commit, index, last), chunk);
    index++;
    produced += chunk.length;
    return ct;
  };
  const sources = (async function* () {
    yield prefix;
    for (let at = 0; at < p.size; at += READ_SLICE) {
      const end = Math.min(p.size, at + READ_SLICE);
      const piece = await p.content(at, end);
      onRead?.(end - at);
      yield piece;
    }
  })();
  for await (const piece of sources) {
    buf = buf.length ? concatBytes(buf, piece) : piece;
    while (buf.length >= CHUNK_PLAINTEXT && produced + CHUNK_PLAINTEXT <= total) {
      yield await emit(buf.subarray(0, CHUNK_PLAINTEXT));
      buf = buf.subarray(CHUNK_PLAINTEXT);
    }
  }
  // Padding: zeros up to the padded length.
  while (produced < total) {
    const want = Math.min(CHUNK_PLAINTEXT, total - produced);
    const chunk = new Uint8Array(want);
    chunk.set(buf.subarray(0, Math.min(buf.length, want)));
    buf = buf.subarray(Math.min(buf.length, want));
    yield await emit(chunk);
  }
  if (index !== p.chunks) throw new Error("internal: chunk count mismatch");
}

/**
 * The hosting site manifest for a sealed blob (one file "o"), byte-exact with
 * serde_json of the Rust SiteManifest, from the BLAKE3 of each 1 MiB chunk.
 */
export function manifestBytes(name, size, chunkHashes) {
  return utf8ToBytes(
    `{"v":1,"name":${JSON.stringify(name)},"files":{"o":{"size":${size},"content_type":"application/octet-stream","chunks":[${chunkHashes
      .map((h) => `"${h}"`)
      .join(",")}]}}}`,
  );
}

/** Sign the v1 hosting record (site.rs SiteRecord::signing_bytes) with the object key. */
export function signRecord(p, manifestHashHex, seq) {
  const msg = utf8ToBytes(`${RECORD_DOMAIN}\n${p.siteId}\n${p.name}\n${p.objOwner}\n${manifestHashHex}\n${seq}\n${p.blobSize}\n`);
  return {
    v: 1,
    site_id: p.siteId,
    name: p.name,
    owner: p.objOwner,
    manifest: manifestHashHex,
    seq,
    size: p.blobSize,
    domains: [],
    sig: bytesToHex(ed25519.sign(msg, p.seed)),
  };
}

/** Running BLAKE3 over 1 MiB boundaries of a byte stream (the manifest's chunk list). */
export function chunkHasher() {
  let h = blake3.create({});
  let filled = 0;
  const hashes = [];
  return {
    update(bytes) {
      let at = 0;
      while (at < bytes.length) {
        const take = Math.min(FILE_CHUNK_BYTES - filled, bytes.length - at);
        h.update(bytes.subarray(at, at + take));
        filled += take;
        at += take;
        if (filled === FILE_CHUNK_BYTES) {
          hashes.push(bytesToHex(h.digest()));
          h = blake3.create({});
          filled = 0;
        }
      }
    },
    finish() {
      if (filled > 0 || hashes.length === 0) hashes.push(bytesToHex(h.digest()));
      return hashes;
    },
  };
}

/**
 * Open a sealed object with its data key (base64url or bytes): verifies the
 * commitment, every chunk, the owner signature and the site id, like the
 * Rust `open`. Returns { meta, content }.
 */
export async function openSealed(blob, key, expectedSiteId) {
  const dek = typeof key === "string" ? fromB64url(key) : key;
  if (blob.length < HEADER_LEN + TAG_LEN || !MAGIC.every((b, i) => blob[i] === b)) throw new Error("not a sealed object");
  const streamId = blob.subarray(8, 24);
  const commit = blob.subarray(24, 56);
  if (bytesToHex(hkdf32(dek, streamId, INFO_COMMIT)) !== bytesToHex(commit)) throw new Error("wrong key");
  const bodyKey = await aesKey(hkdf32(dek, streamId, INFO_BODY));
  const stream = blob.subarray(HEADER_LEN);
  const n = Math.ceil(stream.length / CHUNK_CIPHERTEXT);
  const parts = [];
  for (let i = 0; i < n; i++) {
    const last = i === n - 1;
    const ct = stream.subarray(i * CHUNK_CIPHERTEXT, Math.min(stream.length, (i + 1) * CHUNK_CIPHERTEXT));
    parts.push(await aesGcmDecrypt(bodyKey, chunkNonce(i, last), chunkAad(streamId, commit, i, last), ct));
  }
  const plain = concatBytes(...parts);
  const metaLen = new DataView(plain.buffer, plain.byteOffset).getUint32(0);
  const metaBytes = plain.subarray(4, 4 + metaLen);
  const meta = JSON.parse(new TextDecoder().decode(metaBytes));
  const sig = plain.subarray(4 + metaLen, 4 + metaLen + SIG_LEN);
  const content = plain.slice(4 + metaLen + SIG_LEN, 4 + metaLen + SIG_LEN + meta.size);
  if (siteIdFor(meta.owner, meta.name) !== meta.site_id) throw new Error("site id does not match the owner key");
  const msg = utf8ToBytes(
    `${SIG_DOMAIN}\n${meta.site_id}\n${bytesToHex(streamId)}\n${bytesToHex(commit)}\n${bytesToHex(blake3(metaBytes))}\n${bytesToHex(blake3(content))}`,
  );
  if (!ed25519.verify(sig, msg, hexToBytes(meta.owner))) throw new Error("owner signature does not verify");
  if (expectedSiteId && expectedSiteId !== meta.site_id) throw new Error("not the object requested");
  return { meta, content };
}
