// @profullstack/c0mpute-sealed: publish files into a person's c0mpute
// private storage from an app (browser, React Native, Node), end-to-end
// encrypted. The app seals on the person's device to their linked storage
// identity; neither the app's servers nor c0mpute.com ever hold a key that
// opens it. The person reads it with `c0mpute storage get c0p1:...`.
//
// Auth: an OAuth 2.1 token from c0mpute.com with the `storage:write` scope
// (PKCE, public client: see README), or a c0k_ API key.

import {
  b64url,
  chunkHasher,
  ciphertext,
  finalizePlan,
  manifestBytes,
  PART_BYTES,
  parseIdentity,
  planObject,
  signRecord,
} from "./format.js";
import { blake3 } from "@noble/hashes/blake3.js";
import { bytesToHex } from "@noble/hashes/utils.js";

export * from "./format.js";

export const DEFAULT_BASE_URL = "https://c0mpute.com";
export const DEFAULT_GATEWAY = "https://gateway.c0mpute.com";
/** Bytes per PUT to the ingest gateway. */
export const PIECE_BYTES = 8 * 1024 * 1024;

export class C0mputeStorageError extends Error {
  constructor(status, code, message) {
    super(message);
    this.name = "C0mputeStorageError";
    this.status = status;
    this.code = code;
  }
}

function base(opts) {
  return (opts?.baseUrl ?? DEFAULT_BASE_URL).replace(/\/$/, "");
}

async function api(opts, method, path, body) {
  const f = opts?.fetch ?? globalThis.fetch;
  if (!opts?.token) throw new C0mputeStorageError(401, "unauthorized", "token is required (OAuth storage:write token or c0k_ key)");
  const res = await f(`${base(opts)}${path}`, {
    method,
    headers: { authorization: `Bearer ${opts.token}`, ...(body ? { "content-type": "application/json" } : {}) },
    body: body ? JSON.stringify(body) : undefined,
    signal: opts?.signal,
  });
  const text = await res.text();
  let j = null;
  try {
    j = text ? JSON.parse(text) : null;
  } catch {}
  if (!res.ok) {
    const err = j?.error;
    throw new C0mputeStorageError(res.status, err?.code ?? "http_error", err?.message ?? (typeof err === "string" ? err : `HTTP ${res.status}`));
  }
  return j;
}

/**
 * The person's linked storage identity: { linked, identity: "c0ps1...",
 * x25519 (base64url), fingerprint, ingest_url }. Throws with code
 * "not_linked" (409) when they have not run `c0mpute storage key link`.
 */
export async function identity(opts) {
  return api(opts, "GET", "/api/v1/storage/identity");
}

/** Like identity(), but resolves { linked: false } instead of throwing when nothing is linked. */
export async function linkStatus(opts) {
  try {
    return await identity(opts);
  } catch (e) {
    if (e instanceof C0mputeStorageError && e.code === "not_linked") return { linked: false };
    throw e;
  }
}

/** Replication of an uploaded object: { ref, status, copies, target, objects }. */
export async function status(ref, opts) {
  return api(opts, "GET", `/api/v1/storage/objects/${encodeURIComponent(ref)}`);
}

/**
 * A browser link that opens the object for anyone holding it (the key rides
 * after #, which browsers never send). Treat it like a password.
 */
export function link(ref, key, opts = {}) {
  const id = String(ref).replace(/^c0p1:/, "").split("#")[0];
  if (!/^[0-9a-f]{20}$/.test(id)) throw new Error("expected c0p1:<20 hex>");
  return `${(opts.gateway ?? DEFAULT_GATEWAY).replace(/\/$/, "")}/p/${id}/#k=${key}`;
}

/** A Blob/File, Uint8Array or ArrayBuffer as { size, read(start, end) }. */
function source(input) {
  if (input instanceof Uint8Array) return { size: input.length, read: async (s, e) => input.subarray(s, e) };
  if (input instanceof ArrayBuffer) {
    const u = new Uint8Array(input);
    return { size: u.length, read: async (s, e) => u.subarray(s, e) };
  }
  if (input && typeof input.slice === "function" && typeof input.size === "number") {
    return { size: input.size, read: async (s, e) => new Uint8Array(await input.slice(s, e).arrayBuffer()) };
  }
  throw new TypeError("seal a Blob/File (fs.openAsBlob in Node), a Uint8Array or an ArrayBuffer");
}

/**
 * Plan the sealed objects for a file: one object up to 192 MiB, else parts
 * of 192 MiB plus a root that holds their keys (the Rust layout). Every
 * object is wrapped to `identity` only and carries custody of its signing
 * key for the owner. Returns { ref, siteId, key, objects, totalBytes }; the
 * objects are uploaded in order (parts first, root last).
 */
export async function planSealed(input, { identity: id, fileName, contentType, now = Date.now(), partBytes = PART_BYTES } = {}) {
  const src = source(input);
  const owner = parseIdentity(id);
  const created = Math.floor(now / 1000);
  const version = Math.max(1, now);
  const objects = [];
  let root;
  if (src.size <= partBytes) {
    root = planObject({
      kind: "file",
      size: src.size,
      content: (s, e) => src.read(s, e),
      recipients: [owner],
      owner,
      fileName,
      contentType,
      created,
      version,
    });
  } else {
    const parts = [];
    for (let at = 0; at < src.size; at += partBytes) {
      const start = at;
      const len = Math.min(partBytes, src.size - at);
      const part = planObject({
        kind: "part",
        size: len,
        content: (s, e) => src.read(start + s, start + e),
        recipients: [],
        owner,
        created,
        version,
      });
      objects.push(part);
      parts.push({ site_id: part.siteId, key: b64url(part.dek), len });
    }
    root = planObject({
      kind: "multipart",
      size: 0,
      content: async () => new Uint8Array(0),
      recipients: [owner],
      owner,
      fileName,
      contentType,
      parts,
      created,
      version,
    });
  }
  objects.push(root);
  for (const o of objects) await finalizePlan(o);
  return {
    ref: `c0p1:${root.siteId}`,
    siteId: root.siteId,
    key: b64url(root.dek),
    objects,
    totalBytes: objects.reduce((n, o) => n + o.blobSize, 0),
    contentBytes: src.size,
  };
}

/**
 * Seal one planned object and hand its ciphertext to `write(piece)` in
 * pieces of up to PIECE_BYTES; returns the signed hosting record.
 */
export async function streamObject(p, write, { onRead, onWrite, pieceBytes = PIECE_BYTES } = {}) {
  const hasher = chunkHasher();
  let pending = [];
  let pendingLen = 0;
  let sent = 0;
  const flush = async () => {
    if (!pendingLen) return;
    const piece = new Uint8Array(pendingLen);
    let at = 0;
    for (const b of pending) {
      piece.set(b, at);
      at += b.length;
    }
    pending = [];
    pendingLen = 0;
    await write(piece, sent);
    sent += piece.length;
    onWrite?.(piece.length);
  };
  for await (const ct of ciphertext(p, { onRead })) {
    hasher.update(ct);
    pending.push(ct);
    pendingLen += ct.length;
    if (pendingLen >= pieceBytes) await flush();
  }
  await flush();
  if (sent !== p.blobSize) throw new Error(`internal: sealed ${sent} bytes, planned ${p.blobSize}`);
  const manifest = manifestBytes(p.name, p.blobSize, hasher.finish());
  return signRecord(p, bytesToHex(blake3(manifest)), Date.now());
}

async function putPiece(f, url, ticket, piece, offset, signal) {
  for (let attempt = 0; ; attempt++) {
    let res;
    try {
      res = await f(`${url}?offset=${offset}`, {
        method: "PUT",
        headers: { authorization: `Bearer ${ticket}`, "content-type": "application/octet-stream" },
        body: piece,
        signal,
      });
    } catch (e) {
      if (signal?.aborted || attempt >= 5) throw e;
      res = null;
    }
    if (res?.ok) return;
    const j = res ? await res.json().catch(() => ({})) : {};
    if (res?.status === 409 && typeof j.received === "number") {
      if (j.received === offset + piece.length) return; // it landed before the connection dropped
      if (j.received !== offset) throw new C0mputeStorageError(409, "offset_mismatch", `gateway has ${j.received} bytes, expected ${offset}`);
    } else if (res && res.status < 500 && res.status !== 429) {
      throw new C0mputeStorageError(res.status, "ingest_refused", j.error ?? `HTTP ${res.status}`);
    }
    if (attempt >= 5) throw new C0mputeStorageError(res?.status ?? 0, "ingest_failed", j.error ?? "the gateway kept failing");
    await new Promise((r) => setTimeout(r, 500 * 2 ** attempt));
    // Ask what arrived before retrying (a lost response after a write).
    try {
      const s = await (await f(url, { headers: { authorization: `Bearer ${ticket}` }, signal })).json();
      if (s.received === offset + piece.length) return;
    } catch {}
  }
}

/**
 * Seal `file` to the token owner's linked storage identity and publish it to
 * their c0mpute private storage. Returns { ref, siteId, key, link,
 * uploadId, status, copies }. `key` and `link` open the object: show them to
 * the person if useful, never store or send them anywhere.
 *
 * opts: { token, baseUrl?, fileName?, contentType?, onProgress?, signal?, fetch? }
 * onProgress({ phase: "hashing"|"uploading"|"publishing"|"done", done, total, object, objects })
 */
export async function uploadSealed(file, opts = {}) {
  const f = opts.fetch ?? globalThis.fetch;
  const who = await identity(opts);
  const plan = await planSealed(file, {
    identity: who.identity,
    fileName: opts.fileName ?? (typeof file?.name === "string" ? file.name : undefined),
    contentType: opts.contentType ?? (typeof file?.type === "string" && file.type ? file.type : undefined),
  });
  const up = await api(opts, "POST", "/api/v1/storage/uploads", { bytes: plan.totalBytes, objects: plan.objects.length });
  const ingest = (up.ingest_url ?? who.ingest_url ?? DEFAULT_GATEWAY).replace(/\/$/, "");
  // Each byte of content is read twice (hash, then encrypt) and each
  // ciphertext byte is sent once.
  const total = plan.contentBytes * 2 + plan.totalBytes;
  let done = 0;
  const progress = (phase, object) => opts.onProgress?.({ phase, done, total, object, objects: plan.objects.length });
  for (const [i, p] of plan.objects.entries()) {
    const url = `${ingest}/storage/v1/sealed/${p.siteId}`;
    let hashed = 0;
    const record = await streamObject(p, (piece, offset) => putPiece(f, url, up.ticket, piece, offset, opts.signal), {
      onRead: (n) => {
        done += n;
        hashed += n;
        progress(hashed <= p.size ? "hashing" : "uploading", i);
      },
      onWrite: (n) => {
        done += n;
        progress("uploading", i);
      },
    });
    progress("publishing", i);
    const res = await f(`${url}/finish`, {
      method: "POST",
      headers: { authorization: `Bearer ${up.ticket}`, "content-type": "application/json" },
      body: JSON.stringify({ record }),
      signal: opts.signal,
    });
    if (!res.ok) {
      const j = await res.json().catch(() => ({}));
      throw new C0mputeStorageError(res.status, "publish_refused", j.error ?? `HTTP ${res.status}`);
    }
  }
  const fin = await api(opts, "POST", `/api/v1/storage/uploads/${up.upload_id}`, {
    ref: plan.ref,
    site_ids: plan.objects.map((o) => o.siteId),
  });
  progress("done", plan.objects.length - 1);
  return {
    ref: plan.ref,
    siteId: plan.siteId,
    key: plan.key,
    link: link(plan.ref, plan.key),
    uploadId: up.upload_id,
    status: fin?.status ?? "publishing",
    copies: fin?.copies ?? 0,
    bytes: plan.totalBytes,
  };
}

