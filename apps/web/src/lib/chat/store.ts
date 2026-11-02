// Accounts for c0mpute chat: CoinPay-only sign-in, one row per CoinPay `sub`,
// and hashed bearer tokens (browser sessions, CLI/API keys, device codes).
//
// Storage is the site's SQLite Cloud database (SQLITE_CLOUD_URL). The store is
// written against a two-method `Sql` interface so tests run it on bun:sqlite.

import { createHash, randomBytes } from "node:crypto";

export type Row = Record<string, unknown>;
export interface Sql {
  all(query: string, params?: unknown[]): Promise<Row[]>;
  run(query: string, params?: unknown[]): Promise<void>;
}

export type User = { id: string; coinpay_sub: string; did: string; name: string | null };
export type TokenKind = "session" | "key";
export type Token = {
  token_hash: string;
  user_id: string;
  kind: TokenKind;
  label: string | null;
  created_at: string;
  expires_at: string | null;
  last_used_at: string | null;
};

const SCHEMA = [
  `CREATE TABLE IF NOT EXISTS chat_users (
     id            TEXT PRIMARY KEY,
     coinpay_sub   TEXT NOT NULL UNIQUE,
     did           TEXT NOT NULL,
     name          TEXT,
     created_at    TEXT NOT NULL,
     last_login_at TEXT NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS chat_tokens (
     token_hash   TEXT PRIMARY KEY,
     user_id      TEXT NOT NULL,
     kind         TEXT NOT NULL,
     label        TEXT,
     prefix       TEXT,
     created_at   TEXT NOT NULL,
     expires_at   TEXT,
     last_used_at TEXT
   )`,
  `CREATE INDEX IF NOT EXISTS idx_chat_tokens_user ON chat_tokens(user_id)`,
  `CREATE TABLE IF NOT EXISTS chat_device_codes (
     device_hash TEXT PRIMARY KEY,
     user_code   TEXT NOT NULL UNIQUE,
     user_id     TEXT,
     label       TEXT,
     expires_at  TEXT NOT NULL
   )`,
];

export const SESSION_PREFIX = "c0s_";
export const KEY_PREFIX = "c0k_";
export const DEVICE_PREFIX = "c0d_";
const SESSION_DAYS = 30;
const DEVICE_MINUTES = 10;
// No 0/O/1/I so a code read off one screen and typed into another survives.
const USER_CODE_ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

export const hashToken = (t: string) => createHash("sha256").update(t).digest("hex");
const newSecret = (prefix: string) => prefix + randomBytes(24).toString("base64url");
const now = () => new Date().toISOString();
const plus = (ms: number) => new Date(Date.now() + ms).toISOString();

/** A DID as CoinPay issues it: `did:<method>:<id>`. */
export function isValidDid(did: unknown): did is string {
  return typeof did === "string" && did.length <= 512 && /^did:[a-z0-9]+:[A-Za-z0-9._:%-]+$/.test(did);
}

export function userCode(): string {
  const b = randomBytes(8);
  let s = "";
  for (let i = 0; i < 8; i++) s += USER_CODE_ALPHABET[b[i] % USER_CODE_ALPHABET.length];
  return `${s.slice(0, 4)}-${s.slice(4)}`;
}

export function normalizeUserCode(raw: string): string {
  const s = raw.toUpperCase().replace(/[^A-Z0-9]/g, "");
  return s.length === 8 ? `${s.slice(0, 4)}-${s.slice(4)}` : "";
}

export function createStore(sql: Sql) {
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  async function one<T>(q: string, p: unknown[]): Promise<T | null> {
    await init();
    const rows = await sql.all(q, p);
    return (rows[0] as T) ?? null;
  }

  async function issue(userId: string, kind: TokenKind, label: string | null, prefix: string, ttlMs: number | null) {
    const token = newSecret(prefix);
    await sql.run(
      `INSERT INTO chat_tokens (token_hash, user_id, kind, label, prefix, created_at, expires_at)
       VALUES (?, ?, ?, ?, ?, ?, ?)`,
      [hashToken(token), userId, kind, label, token.slice(0, 8), now(), ttlMs ? plus(ttlMs) : null],
    );
    return token;
  }

  return {
    init,

    /**
     * Sign-in from a verified CoinPay userinfo response. Binds by `sub` only
     * (CoinPay emails are unverified, so they never link accounts) and refuses
     * an account without a DID.
     */
    async upsertCoinpayUser(info: { sub: string; did?: unknown; name?: unknown }): Promise<User> {
      if (!info.sub) throw new Error("CoinPay userinfo has no sub");
      if (!isValidDid(info.did)) throw new NoDidError();
      const name = typeof info.name === "string" ? info.name.slice(0, 200) : null;
      const existing = await one<User>(`SELECT * FROM chat_users WHERE coinpay_sub = ?`, [info.sub]);
      if (existing) {
        await sql.run(`UPDATE chat_users SET did = ?, name = ?, last_login_at = ? WHERE id = ?`, [
          info.did,
          name,
          now(),
          existing.id,
        ]);
        return { ...existing, did: info.did, name };
      }
      const id = "u_" + randomBytes(12).toString("hex");
      await sql.run(
        `INSERT INTO chat_users (id, coinpay_sub, did, name, created_at, last_login_at) VALUES (?, ?, ?, ?, ?, ?)`,
        [id, info.sub, info.did, name, now(), now()],
      );
      return { id, coinpay_sub: info.sub, did: info.did, name };
    },

    createSession: (userId: string) =>
      init().then(() => issue(userId, "session", "browser", SESSION_PREFIX, SESSION_DAYS * 86_400_000)),

    createKey: (userId: string, label: string) =>
      init().then(() => issue(userId, "key", label.slice(0, 80) || "key", KEY_PREFIX, null)),

    /** The user behind a session cookie or API key, or null. Touches last_used_at. */
    async userForToken(token: string | null | undefined): Promise<(User & { token_kind: TokenKind }) | null> {
      if (!token || !(token.startsWith(SESSION_PREFIX) || token.startsWith(KEY_PREFIX))) return null;
      const h = hashToken(token);
      const row = await one<User & { token_kind: TokenKind; expires_at: string | null }>(
        `SELECT u.*, t.kind AS token_kind, t.expires_at
           FROM chat_tokens t JOIN chat_users u ON u.id = t.user_id
          WHERE t.token_hash = ?`,
        [h],
      );
      if (!row) return null;
      if (row.expires_at && row.expires_at < now()) {
        await sql.run(`DELETE FROM chat_tokens WHERE token_hash = ?`, [h]);
        return null;
      }
      if (!isValidDid(row.did)) return null;
      sql.run(`UPDATE chat_tokens SET last_used_at = ? WHERE token_hash = ?`, [now(), h]).catch(() => {});
      const { expires_at: _e, ...user } = row;
      return user;
    },

    revokeToken: async (token: string) => {
      await init();
      await sql.run(`DELETE FROM chat_tokens WHERE token_hash = ?`, [hashToken(token)]);
    },

    async listKeys(userId: string) {
      await init();
      const rows = await sql.all(
        `SELECT token_hash, label, prefix, created_at, last_used_at FROM chat_tokens
          WHERE user_id = ? AND kind = 'key' ORDER BY created_at DESC`,
        [userId],
      );
      // The hash is the key's public id: revoking needs it, using it does not.
      return rows.map((r) => ({
        id: String(r.token_hash).slice(0, 16),
        label: r.label as string,
        prefix: r.prefix as string,
        created_at: r.created_at as string,
        last_used_at: (r.last_used_at as string) ?? null,
      }));
    },

    async revokeKeyById(userId: string, id: string) {
      await init();
      if (!/^[0-9a-f]{16}$/.test(id)) return false;
      const rows = await sql.all(
        `SELECT token_hash FROM chat_tokens WHERE user_id = ? AND kind = 'key' AND substr(token_hash, 1, 16) = ?`,
        [userId, id],
      );
      for (const r of rows) await sql.run(`DELETE FROM chat_tokens WHERE token_hash = ?`, [r.token_hash]);
      return rows.length > 0;
    },

    // ── CLI device flow (RFC 8628 shape) ──────────────────────────────────────
    async startDevice(label: string) {
      await init();
      await sql.run(`DELETE FROM chat_device_codes WHERE expires_at < ?`, [now()]);
      const device = newSecret(DEVICE_PREFIX);
      const code = userCode();
      await sql.run(
        `INSERT INTO chat_device_codes (device_hash, user_code, label, expires_at) VALUES (?, ?, ?, ?)`,
        [hashToken(device), code, label.slice(0, 80) || "cli", plus(DEVICE_MINUTES * 60_000)],
      );
      return { device_code: device, user_code: code, expires_in: DEVICE_MINUTES * 60 };
    },

    async approveDevice(userCodeRaw: string, userId: string) {
      const code = normalizeUserCode(userCodeRaw);
      if (!code) return false;
      const row = await one<{ device_hash: string; user_id: string | null; expires_at: string }>(
        `SELECT device_hash, user_id, expires_at FROM chat_device_codes WHERE user_code = ?`,
        [code],
      );
      if (!row || row.expires_at < now() || row.user_id) return false;
      await sql.run(`UPDATE chat_device_codes SET user_id = ? WHERE device_hash = ?`, [userId, row.device_hash]);
      return true;
    },

    async deviceLabel(userCodeRaw: string) {
      const code = normalizeUserCode(userCodeRaw);
      if (!code) return null;
      const row = await one<{ label: string; expires_at: string; user_id: string | null }>(
        `SELECT label, expires_at, user_id FROM chat_device_codes WHERE user_code = ?`,
        [code],
      );
      return row && row.expires_at >= now() && !row.user_id ? row.label : null;
    },

    /** Poll: "pending" until approved, then a key exactly once. */
    async pollDevice(device: string): Promise<{ status: "pending" | "expired" } | { status: "ok"; key: string }> {
      const h = hashToken(device);
      const row = await one<{ user_id: string | null; label: string; expires_at: string }>(
        `SELECT user_id, label, expires_at FROM chat_device_codes WHERE device_hash = ?`,
        [h],
      );
      if (!row || row.expires_at < now()) return { status: "expired" };
      if (!row.user_id) return { status: "pending" };
      await sql.run(`DELETE FROM chat_device_codes WHERE device_hash = ?`, [h]);
      const key = await issue(row.user_id, "key", row.label, KEY_PREFIX, null);
      return { status: "ok", key };
    },
  };
}

export class NoDidError extends Error {
  constructor() {
    super("This CoinPay account has no DID. Create one at coinpayportal.com, then sign in again.");
  }
}

export type Store = ReturnType<typeof createStore>;
