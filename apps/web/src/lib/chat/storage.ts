// Private storage for apps: a c0mpute.com account (CoinPay identity) is the
// anchor that lets an app the person authorized (OAuth scope storage:write)
// upload encrypted files into that person's c0mpute private storage.
//
// What lives here, and what never does:
// - the person's PUBLIC storage identity (c0ps1...), linked by proving
//   possession of the storage key (an X25519 challenge, see verifyLinkProof);
// - upload sessions: who uploaded how much, when, through which app, and the
//   object ids, for quotas and the replication status endpoint.
// Never a key that decrypts anything: apps seal in the person's browser to the
// linked identity, and the bytes go straight to a gateway with a ticket this
// site signs (signTicket). See docs/private-storage.md, "Publish from apps".

import { createHash, createPrivateKey, createPublicKey, diffieHellman, generateKeyPairSync, hkdfSync, randomBytes, sign, timingSafeEqual } from "node:crypto";
import { CLAIMS_SCHEMA, claimOnce, type Sql } from "./store";

export const IDENTITY_PREFIX = "c0ps1.";
export const STORAGE_SCOPE = "storage:write";
export const LINK_INFO = "c0mpute-storage-link-v1";
export const TICKET_DOMAIN = "c0mpute-sealed-ticket-v1";
/** One sealed object: a 192 MiB part after padding, tags and header (= the gateway's cap). */
export const MAX_OBJECT_BYTES = 200 * 1024 * 1024;
/** Content per part (the format's split size). */
export const PART_BYTES = 192 * 1024 * 1024;
const CHALLENGE_MINUTES = 5;
const TICKET_HOURS = 6;

const X25519_SPKI = Buffer.from("302a300506032b656e032100", "hex");
const X25519_PKCS8 = Buffer.from("302e020100300506032b656e04220420", "hex");
const ED25519_PKCS8 = Buffer.from("302e020100300506032b657004220420", "hex");

export type Limits = {
  /** Most ciphertext bytes in one upload (one file, all its parts). */
  maxUploadBytes: number;
  /** Most bytes one account may start uploading per rolling 24 hours. */
  dailyBytes: number;
  /** Most bytes one account may have uploaded through apps in all. */
  totalBytes: number;
  /** Most upload sessions per account per hour. */
  uploadsPerHour: number;
};

export const DEFAULT_LIMITS: Limits = {
  maxUploadBytes: 4 * 1024 ** 3,
  dailyBytes: 20 * 1024 ** 3,
  totalBytes: 200 * 1024 ** 3,
  uploadsPerHour: 60,
};

const SCHEMA = [
  `CREATE TABLE IF NOT EXISTS storage_identities (
     user_id     TEXT PRIMARY KEY,
     identity    TEXT NOT NULL,
     fingerprint TEXT,
     linked_at   TEXT NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS storage_link_challenges (
     id            TEXT PRIMARY KEY,
     user_id       TEXT NOT NULL,
     server_secret TEXT NOT NULL,
     nonce         TEXT NOT NULL,
     expires_at    TEXT NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS storage_uploads (
     id           TEXT PRIMARY KEY,
     user_id      TEXT NOT NULL,
     app          TEXT,
     bytes        INTEGER NOT NULL,
     objects      INTEGER NOT NULL,
     created_at   TEXT NOT NULL,
     expires_at   TEXT NOT NULL,
     root_site_id TEXT,
     site_ids     TEXT,
     completed_at TEXT
   )`,
  `CREATE INDEX IF NOT EXISTS idx_storage_uploads_user ON storage_uploads(user_id, created_at)`,
  CLAIMS_SCHEMA,
];

const now = () => new Date().toISOString();
const b64u = (b: Buffer | Uint8Array) => Buffer.from(b).toString("base64url");

export class StorageError extends Error {
  constructor(
    public status: number,
    public code: string,
    message: string,
  ) {
    super(message);
  }
}

/** The 32-byte X25519 key of a `c0ps1.<base64url>` identity, or null. */
export function parseIdentity(raw: unknown): Buffer | null {
  if (typeof raw !== "string" || raw.length > 64) return null;
  const s = raw.trim();
  if (!s.startsWith(IDENTITY_PREFIX)) return null;
  const body = s.slice(IDENTITY_PREFIX.length);
  if (!/^[A-Za-z0-9_-]{43}$/.test(body)) return null;
  const key = Buffer.from(body, "base64url");
  if (key.length !== 32 || key.every((b) => b === 0)) return null;
  return key;
}

export function siteIdOk(s: unknown): s is string {
  return typeof s === "string" && /^[0-9a-f]{20}$/.test(s);
}

/** `c0p1:<id>` or a bare 20-hex id, to the id. */
export function refToSiteId(raw: string): string | null {
  const s = decodeURIComponent(raw).trim().replace(/^c0p1:/, "").split("#")[0];
  return siteIdOk(s) ? s : null;
}

function x25519Public(raw: Buffer) {
  return createPublicKey({ key: Buffer.concat([X25519_SPKI, raw]), format: "der", type: "spki" });
}

function x25519Private(raw: Buffer) {
  return createPrivateKey({ key: Buffer.concat([X25519_PKCS8, raw]), format: "der", type: "pkcs8" });
}

/** A fresh X25519 key pair as raw 32-byte buffers. */
export function x25519Pair(): { secret: Buffer; public: Buffer } {
  const { privateKey, publicKey } = generateKeyPairSync("x25519");
  const sk = privateKey.export({ format: "der", type: "pkcs8" }) as Buffer;
  const pk = publicKey.export({ format: "der", type: "spki" }) as Buffer;
  return { secret: sk.subarray(sk.length - 32), public: pk.subarray(pk.length - 32) };
}

/** The proof the CLI computes (StorageSecret::link_proof in c0mpute-sealed). */
export function linkProof(serverSecret: Buffer, serverPublic: Buffer, identityKey: Buffer, context: string): Buffer | null {
  let ss: Buffer;
  try {
    ss = diffieHellman({ privateKey: x25519Private(serverSecret), publicKey: x25519Public(identityKey) });
  } catch {
    return null;
  }
  if (ss.every((b) => b === 0)) return null; // low-order point
  const salt = Buffer.concat([serverPublic, identityKey]);
  return Buffer.from(hkdfSync("sha256", ss, salt, Buffer.from(`${LINK_INFO}\n${context}`), 32));
}

export const linkContext = (challengeId: string, nonce: string, identity: string) => `${challengeId}\n${nonce}\n${identity}`;

/** The ticket signing key: C0MPUTE_STORAGE_TICKET_KEY, a 32-byte ed25519 seed in hex. */
export function ticketKey(seedHex: string) {
  if (!/^[0-9a-fA-F]{64}$/.test(seedHex.trim())) return null;
  const key = createPrivateKey({ key: Buffer.concat([ED25519_PKCS8, Buffer.from(seedHex.trim(), "hex")]), format: "der", type: "pkcs8" });
  const spki = createPublicKey(key).export({ format: "der", type: "spki" }) as Buffer;
  return { key, publicHex: spki.subarray(spki.length - 32).toString("hex") };
}

export type Ticket = { v: 1; upload: string; sub: string; max_bytes: number; max_objects: number; exp: number };

/** `<base64url(json)>.<base64url(ed25519 sig over "c0mpute-sealed-ticket-v1\n" + payload)>`. */
export function signTicket(seedHex: string, t: Ticket): string {
  const k = ticketKey(seedHex);
  if (!k) throw new StorageError(503, "not_configured", "app uploads are not configured on this server (no ticket key)");
  const payload = b64u(Buffer.from(JSON.stringify(t)));
  const sig = sign(null, Buffer.from(`${TICKET_DOMAIN}\n${payload}`), k.key);
  return `${payload}.${b64u(sig)}`;
}

/** Objects an upload of `bytes` ciphertext may need: its parts plus the root. */
export function objectsFor(bytes: number): number {
  return Math.max(1, Math.ceil(bytes / PART_BYTES)) + 1;
}

export type UploadRow = {
  id: string;
  app: string | null;
  bytes: number;
  objects: number;
  created_at: string;
  expires_at: string;
  root_site_id: string | null;
  site_ids: string[];
  completed_at: string | null;
};

function toUpload(r: Record<string, unknown>): UploadRow {
  let ids: string[] = [];
  try {
    ids = JSON.parse(String(r.site_ids ?? "[]"));
  } catch {}
  return {
    id: String(r.id),
    app: (r.app as string | null) ?? null,
    bytes: Number(r.bytes),
    objects: Number(r.objects),
    created_at: String(r.created_at),
    expires_at: String(r.expires_at),
    root_site_id: (r.root_site_id as string | null) ?? null,
    site_ids: Array.isArray(ids) ? ids.filter(siteIdOk) : [],
    completed_at: (r.completed_at as string | null) ?? null,
  };
}

export function createStorageStore(sql: Sql, opts: { ticketSeed: () => string; limits?: Partial<Limits> } = { ticketSeed: () => "" }) {
  const limits: Limits = { ...DEFAULT_LIMITS, ...(opts.limits ?? {}) };
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  return {
    init,
    limits,

    async identity(userId: string): Promise<{ identity: string; fingerprint: string | null; linked_at: string } | null> {
      await init();
      const r = (await sql.all(`SELECT identity, fingerprint, linked_at FROM storage_identities WHERE user_id = ?`, [userId]))[0];
      return r ? { identity: String(r.identity), fingerprint: (r.fingerprint as string | null) ?? null, linked_at: String(r.linked_at) } : null;
    },

    /** Step 1 of linking: a fresh server X25519 key and nonce, valid 5 minutes. */
    async startLink(userId: string) {
      await init();
      await sql.run(`DELETE FROM storage_link_challenges WHERE expires_at < ?`, [now()]);
      const pair = x25519Pair();
      const id = "lc_" + randomBytes(12).toString("hex");
      const nonce = b64u(randomBytes(32));
      await sql.run(`INSERT INTO storage_link_challenges (id, user_id, server_secret, nonce, expires_at) VALUES (?, ?, ?, ?, ?)`, [
        id,
        userId,
        b64u(pair.secret),
        nonce,
        new Date(Date.now() + CHALLENGE_MINUTES * 60_000).toISOString(),
      ]);
      return { challenge_id: id, server_key: b64u(pair.public), nonce, expires_in: CHALLENGE_MINUTES * 60 };
    },

    /** Step 2: check the proof (once) and store the public identity on the account. */
    async finishLink(userId: string, body: { challenge_id?: unknown; identity?: unknown; proof?: unknown; fingerprint?: unknown }) {
      await init();
      const key = parseIdentity(body.identity);
      if (!key) throw new StorageError(400, "invalid_identity", "identity must be a c0ps1... storage identity (c0mpute storage key show)");
      if (typeof body.challenge_id !== "string" || !/^lc_[0-9a-f]{24}$/.test(body.challenge_id)) throw new StorageError(400, "invalid_challenge", "unknown challenge");
      const r = (await sql.all(`SELECT * FROM storage_link_challenges WHERE id = ?`, [body.challenge_id]))[0];
      if (!r || String(r.user_id) !== userId || !(await claimOnce(sql, `storage-link:${body.challenge_id}`))) {
        throw new StorageError(400, "invalid_challenge", "unknown or used challenge: run `c0mpute storage key link` again");
      }
      await sql.run(`DELETE FROM storage_link_challenges WHERE id = ?`, [body.challenge_id]);
      if (String(r.expires_at) < now()) throw new StorageError(400, "expired_challenge", "the challenge expired: run `c0mpute storage key link` again");
      const secret = Buffer.from(String(r.server_secret), "base64url");
      const serverPublic = createPublicKey(x25519Private(secret)).export({ format: "der", type: "spki" }) as Buffer;
      const identity = `${IDENTITY_PREFIX}${b64u(key)}`;
      const want = linkProof(secret, serverPublic.subarray(serverPublic.length - 32), key, linkContext(body.challenge_id, String(r.nonce), identity));
      const got = typeof body.proof === "string" && /^[A-Za-z0-9_-]{43}$/.test(body.proof) ? Buffer.from(body.proof, "base64url") : Buffer.alloc(0);
      if (!want || got.length !== 32 || !timingSafeEqual(want, got)) {
        throw new StorageError(403, "bad_proof", "the proof does not match: this machine does not hold that storage key");
      }
      const fp = typeof body.fingerprint === "string" && /^([0-9a-f]{2}:){7}[0-9a-f]{2}$/.test(body.fingerprint) ? body.fingerprint : null;
      const at = now();
      await sql.run(`DELETE FROM storage_identities WHERE user_id = ?`, [userId]);
      await sql.run(`INSERT INTO storage_identities (user_id, identity, fingerprint, linked_at) VALUES (?, ?, ?, ?)`, [userId, identity, fp, at]);
      return { identity, fingerprint: fp, linked_at: at };
    },

    async unlink(userId: string) {
      await init();
      await sql.run(`DELETE FROM storage_identities WHERE user_id = ?`, [userId]);
    },

    /**
     * Open an upload session for `bytes` of ciphertext (one file: its parts
     * and root), within the account's limits, and sign its gateway ticket.
     */
    async startUpload(userId: string, body: { bytes?: unknown; objects?: unknown }, app: string | null, at = Date.now()) {
      await init();
      const linked = await this.identity(userId);
      if (!linked) throw new StorageError(409, "not_linked", "this account has no storage identity linked yet: the person runs `c0mpute storage key link`");
      const bytes = Number(body.bytes);
      if (!Number.isSafeInteger(bytes) || bytes <= 0) throw new StorageError(400, "invalid_request", "bytes: the total ciphertext size of the upload (all objects)");
      if (bytes > limits.maxUploadBytes) throw new StorageError(413, "too_large", `one upload may be at most ${limits.maxUploadBytes} bytes`);
      const maxObjects = objectsFor(bytes);
      const objects = body.objects === undefined ? maxObjects : Number(body.objects);
      if (!Number.isSafeInteger(objects) || objects < 1 || objects > maxObjects) {
        throw new StorageError(400, "invalid_request", `objects: 1-${maxObjects} for ${bytes} bytes`);
      }
      const hourAgo = new Date(at - 3_600_000).toISOString();
      const dayAgo = new Date(at - 86_400_000).toISOString();
      const recent = await sql.all(`SELECT COUNT(*) AS n FROM storage_uploads WHERE user_id = ? AND created_at >= ?`, [userId, hourAgo]);
      if (Number(recent[0]?.n ?? 0) >= limits.uploadsPerHour) throw new StorageError(429, "rate_limited", `at most ${limits.uploadsPerHour} uploads per hour`);
      const day = await sql.all(`SELECT COALESCE(SUM(bytes), 0) AS b FROM storage_uploads WHERE user_id = ? AND created_at >= ?`, [userId, dayAgo]);
      if (Number(day[0]?.b ?? 0) + bytes > limits.dailyBytes) throw new StorageError(429, "quota_exceeded", `at most ${limits.dailyBytes} bytes of uploads per 24 hours`);
      const all = await sql.all(`SELECT COALESCE(SUM(bytes), 0) AS b FROM storage_uploads WHERE user_id = ? AND (completed_at IS NOT NULL OR expires_at >= ?)`, [
        userId,
        new Date(at).toISOString(),
      ]);
      if (Number(all[0]?.b ?? 0) + bytes > limits.totalBytes) throw new StorageError(429, "quota_exceeded", `at most ${limits.totalBytes} bytes uploaded through apps in all`);
      const id = "up_" + randomBytes(12).toString("hex");
      const exp = Math.floor(at / 1000) + TICKET_HOURS * 3600;
      const ticket = signTicket(opts.ticketSeed(), {
        v: 1,
        upload: id,
        sub: createHash("sha256").update(`acct|${userId}`).digest("hex").slice(0, 16),
        max_bytes: bytes,
        max_objects: objects,
        exp,
      });
      const createdAt = new Date(at).toISOString();
      const expiresAt = new Date(exp * 1000).toISOString();
      await sql.run(`INSERT INTO storage_uploads (id, user_id, app, bytes, objects, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?)`, [
        id,
        userId,
        app ? app.slice(0, 80) : null,
        bytes,
        objects,
        createdAt,
        expiresAt,
      ]);
      return { upload_id: id, ticket, identity: linked.identity, max_bytes: bytes, max_objects: objects, expires_at: expiresAt };
    },

    async upload(userId: string, id: string): Promise<UploadRow | null> {
      await init();
      if (!/^up_[0-9a-f]{24}$/.test(id)) return null;
      const r = (await sql.all(`SELECT * FROM storage_uploads WHERE id = ? AND user_id = ?`, [id, userId]))[0];
      return r ? toUpload(r) : null;
    },

    async uploadByRef(userId: string, siteId: string): Promise<UploadRow | null> {
      await init();
      if (!siteIdOk(siteId)) return null;
      const r = (await sql.all(`SELECT * FROM storage_uploads WHERE user_id = ? AND root_site_id = ? ORDER BY created_at DESC LIMIT 1`, [userId, siteId]))[0];
      return r ? toUpload(r) : null;
    },

    /** The app reports what it published: the root ref and every site id (parts included). */
    async completeUpload(userId: string, id: string, body: { ref?: unknown; site_ids?: unknown }) {
      const up = await this.upload(userId, id);
      if (!up) throw new StorageError(404, "not_found", "no such upload on this account");
      const root = typeof body.ref === "string" ? refToSiteId(body.ref) : null;
      if (!root) throw new StorageError(400, "invalid_request", "ref: the c0p1:<id> the upload produced");
      const ids = Array.isArray(body.site_ids) ? body.site_ids.filter(siteIdOk) : [];
      const all = [...new Set([...ids, root])];
      if (all.length > up.objects) throw new StorageError(400, "invalid_request", `this upload may hold at most ${up.objects} objects`);
      await sql.run(`UPDATE storage_uploads SET root_site_id = ?, site_ids = ?, completed_at = ? WHERE id = ? AND user_id = ?`, [
        root,
        JSON.stringify(all),
        now(),
        id,
        userId,
      ]);
      return { ...up, root_site_id: root, site_ids: all, completed_at: now() };
    },

    async listUploads(userId: string, limit = 100): Promise<UploadRow[]> {
      await init();
      const rows = await sql.all(`SELECT * FROM storage_uploads WHERE user_id = ? AND completed_at IS NOT NULL ORDER BY created_at DESC LIMIT ?`, [
        userId,
        Math.min(Math.max(limit, 1), 500),
      ]);
      return rows.map(toUpload);
    },
  };
}

export type StorageStore = ReturnType<typeof createStorageStore>;

/** Live copy counts from the ingest gateway for every object of an upload. */
export async function replicationStatus(endpoint: string, siteIds: string[], fetcher: typeof fetch = fetch) {
  const objects = await Promise.all(
    siteIds.slice(0, 64).map(async (id) => {
      try {
        const r = await fetcher(`${endpoint.replace(/\/$/, "")}/storage/v1/sealed/${id}`, { signal: AbortSignal.timeout(8000), cache: "no-store" });
        if (!r.ok) return { site_id: id, published: false, copies: 0 };
        const j = (await r.json()) as { published?: boolean; copies?: number; target?: number };
        return { site_id: id, published: !!j.published, copies: Number(j.copies ?? 0), target: Number(j.target ?? 3) };
      } catch {
        return { site_id: id, published: false, copies: 0, error: "gateway unreachable" };
      }
    }),
  );
  const target = objects.reduce((t, o) => Math.max(t, Number((o as { target?: number }).target ?? 3)), 3);
  const copies = objects.length ? Math.min(...objects.map((o) => o.copies)) : 0;
  const published = objects.length > 0 && objects.every((o) => o.published);
  const status = !published ? "publishing" : copies >= target ? "replicated" : "replicating";
  return { status, copies, target, objects };
}

export const STORAGE_CORS = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET, POST, OPTIONS",
  "access-control-allow-headers": "authorization, content-type",
};
