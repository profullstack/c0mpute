// Account-side tables beside chat_users: passkeys, one-shot WebAuthn and node
// link challenges, fleet enrollment tokens, node tags and the fleet audit log.
//
// Same SQLite Cloud database and the same two-method `Sql` interface as the
// chat store, so tests run on bun:sqlite.

import { randomBytes } from "node:crypto";
import { CLAIMS_SCHEMA, claimOnce, hashToken, type Row, type Sql } from "../chat/store";

export const ENROLL_PREFIX = "c0e_";
/** How long a WebAuthn ceremony or a node link challenge stays answerable. */
export const CHALLENGE_MS = 5 * 60_000;
export const MAX_PASSKEYS = 20;
export const MAX_ENROLL_TOKENS = 50;
export const MAX_TAGS = 12;

const SCHEMA = [
  `CREATE TABLE IF NOT EXISTS account_passkeys (
     credential_id TEXT PRIMARY KEY,
     user_id       TEXT NOT NULL,
     public_key    TEXT NOT NULL,
     counter       INTEGER NOT NULL DEFAULT 0,
     transports    TEXT,
     device_type   TEXT,
     backed_up     INTEGER NOT NULL DEFAULT 0,
     name          TEXT,
     created_at    TEXT NOT NULL,
     last_used_at  TEXT
   )`,
  `CREATE INDEX IF NOT EXISTS idx_account_passkeys_user ON account_passkeys(user_id)`,
  `CREATE TABLE IF NOT EXISTS account_challenges (
     id         TEXT PRIMARY KEY,
     kind       TEXT NOT NULL,
     user_id    TEXT,
     subject    TEXT,
     challenge  TEXT NOT NULL,
     expires_at TEXT NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS fleet_enroll_tokens (
     token_hash TEXT PRIMARY KEY,
     user_id    TEXT NOT NULL,
     name       TEXT NOT NULL,
     prefix     TEXT NOT NULL,
     tags       TEXT,
     max_uses   INTEGER,
     uses       INTEGER NOT NULL DEFAULT 0,
     created_at TEXT NOT NULL,
     expires_at TEXT,
     revoked_at TEXT,
     last_used_at TEXT
   )`,
  `CREATE INDEX IF NOT EXISTS idx_fleet_enroll_user ON fleet_enroll_tokens(user_id)`,
  `CREATE TABLE IF NOT EXISTS fleet_node_meta (
     user_id     TEXT NOT NULL,
     node        TEXT NOT NULL,
     peer_id     TEXT NOT NULL,
     tags        TEXT NOT NULL DEFAULT '[]',
     verified_at TEXT NOT NULL,
     via         TEXT NOT NULL,
     info        TEXT,
     PRIMARY KEY (user_id, node)
   )`,
  `CREATE INDEX IF NOT EXISTS idx_fleet_node_meta_node ON fleet_node_meta(node)`,
  `CREATE TABLE IF NOT EXISTS fleet_audit (
     id         TEXT PRIMARY KEY,
     user_id    TEXT NOT NULL,
     node       TEXT,
     action     TEXT NOT NULL,
     args       TEXT,
     actor      TEXT NOT NULL,
     outcome    TEXT NOT NULL,
     at         TEXT NOT NULL
   )`,
  `CREATE INDEX IF NOT EXISTS idx_fleet_audit_user_at ON fleet_audit(user_id, at)`,
  CLAIMS_SCHEMA,
];

const now = () => new Date().toISOString();
const plus = (ms: number) => new Date(Date.now() + ms).toISOString();

export type Passkey = {
  credential_id: string;
  user_id: string;
  public_key: string;
  counter: number;
  transports: string[];
  device_type: string | null;
  backed_up: boolean;
  name: string | null;
  created_at: string;
  last_used_at: string | null;
};

export type ChallengeKind = "webauthn-register" | "webauthn-login" | "node-link";

export type EnrollToken = {
  id: string;
  name: string;
  prefix: string;
  tags: string[];
  max_uses: number | null;
  uses: number;
  created_at: string;
  expires_at: string | null;
  revoked_at: string | null;
  last_used_at: string | null;
};

export type NodeMeta = { node: string; peer_id: string; tags: string[]; verified_at: string; via: string; info: Record<string, unknown> | null };

/** Tags are short lowercase words: "vast", "home", "rack-a". */
export function cleanTags(raw: unknown): string[] | null {
  const list = Array.isArray(raw) ? raw : typeof raw === "string" ? raw.split(",") : null;
  if (!list) return null;
  const out: string[] = [];
  for (const t of list) {
    if (typeof t !== "string") return null;
    const s = t.trim().toLowerCase();
    if (!s) continue;
    if (!/^[a-z0-9][a-z0-9._-]{0,31}$/.test(s)) return null;
    if (!out.includes(s)) out.push(s);
  }
  return out.length > MAX_TAGS ? null : out;
}

function parseJson<T>(v: unknown, fallback: T): T {
  if (typeof v !== "string") return fallback;
  try {
    return JSON.parse(v) as T;
  } catch {
    return fallback;
  }
}

/** "7d", "12h", "30m", or a number of seconds; null for no expiry. Max one year. */
export function parseTtl(raw: unknown): number | null | "invalid" {
  if (raw == null || raw === "" || raw === "never") return null;
  const m = /^(\d{1,6})\s*([smhd]?)$/i.exec(String(raw).trim());
  if (!m) return "invalid";
  const n = Number(m[1]);
  const unit = { "": 1, s: 1, m: 60, h: 3600, d: 86400 }[m[2].toLowerCase() as "" | "s" | "m" | "h" | "d"];
  const ms = n * unit * 1000;
  if (ms <= 0 || ms > 366 * 86_400_000) return "invalid";
  return ms;
}

export function createAccountStore(sql: Sql) {
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  const toPasskey = (r: Row): Passkey => ({
    credential_id: String(r.credential_id),
    user_id: String(r.user_id),
    public_key: String(r.public_key),
    counter: Number(r.counter) || 0,
    transports: parseJson<string[]>(r.transports, []),
    device_type: (r.device_type as string) ?? null,
    backed_up: Number(r.backed_up) === 1,
    name: (r.name as string) ?? null,
    created_at: String(r.created_at),
    last_used_at: (r.last_used_at as string) ?? null,
  });

  const toEnroll = (r: Row): EnrollToken => ({
    id: String(r.token_hash).slice(0, 16),
    name: String(r.name),
    prefix: String(r.prefix),
    tags: parseJson<string[]>(r.tags, []),
    max_uses: r.max_uses == null ? null : Number(r.max_uses),
    uses: Number(r.uses) || 0,
    created_at: String(r.created_at),
    expires_at: (r.expires_at as string) ?? null,
    revoked_at: (r.revoked_at as string) ?? null,
    last_used_at: (r.last_used_at as string) ?? null,
  });

  return {
    init,

    // ── one-shot challenges (WebAuthn ceremonies, node links) ─────────────────
    /**
     * Store a challenge and return its id (what the browser keeps in a cookie,
     * or the CLI sends back). `subject` binds it further: the peer id a node
     * link is for.
     */
    async putChallenge(kind: ChallengeKind, challenge: string, opts: { userId?: string | null; subject?: string | null } = {}) {
      await init();
      await sql.run(`DELETE FROM account_challenges WHERE expires_at < ?`, [now()]);
      const id = randomBytes(18).toString("base64url");
      await sql.run(
        `INSERT INTO account_challenges (id, kind, user_id, subject, challenge, expires_at) VALUES (?, ?, ?, ?, ?, ?)`,
        [id, kind, opts.userId ?? null, opts.subject ?? null, challenge, plus(CHALLENGE_MS)],
      );
      return id;
    },

    /**
     * Take a challenge exactly once. Null when unknown, expired, of another
     * kind, or already taken: a replayed answer finds nothing.
     */
    async takeChallenge(kind: ChallengeKind, id: string): Promise<{ challenge: string; user_id: string | null; subject: string | null } | null> {
      await init();
      if (typeof id !== "string" || id.length < 16 || id.length > 64) return null;
      const r = (await sql.all(`SELECT * FROM account_challenges WHERE id = ?`, [id]))[0];
      if (!r || r.kind !== kind || String(r.expires_at) < now()) return null;
      if (!(await claimOnce(sql, `challenge:${id}`))) return null;
      await sql.run(`DELETE FROM account_challenges WHERE id = ?`, [id]);
      return { challenge: String(r.challenge), user_id: (r.user_id as string) ?? null, subject: (r.subject as string) ?? null };
    },

    // ── passkeys ──────────────────────────────────────────────────────────────
    async addPasskey(p: Omit<Passkey, "created_at" | "last_used_at">) {
      await init();
      const n = (await sql.all(`SELECT COUNT(*) AS n FROM account_passkeys WHERE user_id = ?`, [p.user_id]))[0];
      if (Number(n?.n) >= MAX_PASSKEYS) throw new Error(`at most ${MAX_PASSKEYS} passkeys per account`);
      await sql.run(
        `INSERT INTO account_passkeys (credential_id, user_id, public_key, counter, transports, device_type, backed_up, name, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
        [
          p.credential_id,
          p.user_id,
          p.public_key,
          p.counter,
          JSON.stringify(p.transports ?? []),
          p.device_type,
          p.backed_up ? 1 : 0,
          p.name ? p.name.slice(0, 80) : null,
          now(),
        ],
      );
    },

    async passkey(credentialId: string): Promise<Passkey | null> {
      await init();
      const r = (await sql.all(`SELECT * FROM account_passkeys WHERE credential_id = ?`, [credentialId]))[0];
      return r ? toPasskey(r) : null;
    },

    async passkeys(userId: string): Promise<Passkey[]> {
      await init();
      return (await sql.all(`SELECT * FROM account_passkeys WHERE user_id = ? ORDER BY created_at`, [userId])).map(toPasskey);
    },

    async usePasskey(credentialId: string, counter: number) {
      await init();
      await sql.run(`UPDATE account_passkeys SET counter = ?, last_used_at = ? WHERE credential_id = ?`, [counter, now(), credentialId]);
    },

    async renamePasskey(userId: string, credentialId: string, name: string) {
      await init();
      await sql.run(`UPDATE account_passkeys SET name = ? WHERE user_id = ? AND credential_id = ?`, [name.slice(0, 80), userId, credentialId]);
    },

    async removePasskey(userId: string, credentialId: string): Promise<boolean> {
      await init();
      const r = await sql.all(`SELECT credential_id FROM account_passkeys WHERE user_id = ? AND credential_id = ?`, [userId, credentialId]);
      if (!r.length) return false;
      await sql.run(`DELETE FROM account_passkeys WHERE user_id = ? AND credential_id = ?`, [userId, credentialId]);
      return true;
    },

    // ── fleet enrollment tokens ───────────────────────────────────────────────
    /** A token a node uses at install time to link itself to this account without a browser. Shown once. */
    async createEnrollToken(userId: string, o: { name: string; ttlMs: number | null; maxUses?: number | null; tags?: string[] }) {
      await init();
      const live = await sql.all(`SELECT COUNT(*) AS n FROM fleet_enroll_tokens WHERE user_id = ? AND revoked_at IS NULL`, [userId]);
      if (Number(live[0]?.n) >= MAX_ENROLL_TOKENS) throw new Error(`at most ${MAX_ENROLL_TOKENS} live enrollment tokens; revoke some`);
      const token = ENROLL_PREFIX + randomBytes(24).toString("base64url");
      await sql.run(
        `INSERT INTO fleet_enroll_tokens (token_hash, user_id, name, prefix, tags, max_uses, uses, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?)`,
        [
          hashToken(token),
          userId,
          o.name.slice(0, 64) || "fleet",
          token.slice(0, 10),
          JSON.stringify(o.tags ?? []),
          o.maxUses ?? null,
          now(),
          o.ttlMs ? plus(o.ttlMs) : null,
        ],
      );
      const row = (await sql.all(`SELECT * FROM fleet_enroll_tokens WHERE token_hash = ?`, [hashToken(token)]))[0];
      return { token, info: toEnroll(row) };
    },

    async enrollTokens(userId: string): Promise<EnrollToken[]> {
      await init();
      return (await sql.all(`SELECT * FROM fleet_enroll_tokens WHERE user_id = ? ORDER BY created_at DESC`, [userId])).map(toEnroll);
    },

    async revokeEnrollToken(userId: string, idOrName: string): Promise<number> {
      await init();
      const rows = await sql.all(
        `SELECT token_hash FROM fleet_enroll_tokens WHERE user_id = ? AND revoked_at IS NULL AND (substr(token_hash, 1, 16) = ? OR name = ?)`,
        [userId, idOrName, idOrName],
      );
      for (const r of rows) await sql.run(`UPDATE fleet_enroll_tokens SET revoked_at = ? WHERE token_hash = ?`, [now(), r.token_hash]);
      return rows.length;
    },

    /**
     * The account a live enrollment token enrolls into, and the tags it gives,
     * or null (unknown, revoked, expired, used up). Does not count a use.
     */
    async enrollTokenOwner(token: string): Promise<{ user_id: string; tags: string[]; hash: string; name: string } | null> {
      await init();
      if (typeof token !== "string" || !token.startsWith(ENROLL_PREFIX)) return null;
      const h = hashToken(token);
      const r = (await sql.all(`SELECT * FROM fleet_enroll_tokens WHERE token_hash = ?`, [h]))[0];
      if (!r || r.revoked_at) return null;
      if (r.expires_at && String(r.expires_at) < now()) return null;
      if (r.max_uses != null && Number(r.uses) >= Number(r.max_uses)) return null;
      return { user_id: String(r.user_id), tags: parseJson<string[]>(r.tags, []), hash: h, name: String(r.name) };
    },

    async countEnrollUse(hash: string) {
      await init();
      await sql.run(`UPDATE fleet_enroll_tokens SET uses = uses + 1, last_used_at = ? WHERE token_hash = ?`, [now(), hash]);
    },

    // ── verified nodes (signed link) ──────────────────────────────────────────
    /**
     * Record a node whose identity key signed a link to `userId`. A node is
     * verified to one account at a time: a newer signed link moves it, since
     * only whoever holds the node's key can make one.
     * Returns the accounts it was moved away from.
     */
    async verifyNode(userId: string, n: { node: string; peer_id: string; via: string; tags?: string[]; info?: Record<string, unknown> | null }): Promise<string[]> {
      await init();
      const others = await sql.all(`SELECT user_id FROM fleet_node_meta WHERE node = ? AND user_id != ?`, [n.node, userId]);
      await sql.run(`DELETE FROM fleet_node_meta WHERE node = ? AND user_id != ?`, [n.node, userId]);
      const prev = (await sql.all(`SELECT tags FROM fleet_node_meta WHERE user_id = ? AND node = ?`, [userId, n.node]))[0];
      const tags = Array.from(new Set([...parseJson<string[]>(prev?.tags, []), ...(n.tags ?? [])])).slice(0, MAX_TAGS);
      await sql.run(
        `INSERT OR REPLACE INTO fleet_node_meta (user_id, node, peer_id, tags, verified_at, via, info) VALUES (?, ?, ?, ?, ?, ?, ?)`,
        [userId, n.node, n.peer_id, JSON.stringify(tags), now(), n.via, n.info ? JSON.stringify(n.info).slice(0, 8000) : null],
      );
      return others.map((r) => String(r.user_id));
    },

    async nodeMeta(userId: string): Promise<NodeMeta[]> {
      await init();
      return (await sql.all(`SELECT * FROM fleet_node_meta WHERE user_id = ?`, [userId])).map((r) => ({
        node: String(r.node),
        peer_id: String(r.peer_id),
        tags: parseJson<string[]>(r.tags, []),
        verified_at: String(r.verified_at),
        via: String(r.via),
        info: parseJson<Record<string, unknown> | null>(r.info, null),
      }));
    },

    /** Which account a node is verified to, if any. */
    async verifiedOwner(node: string): Promise<{ user_id: string; peer_id: string } | null> {
      await init();
      const r = (await sql.all(`SELECT user_id, peer_id FROM fleet_node_meta WHERE node = ?`, [node]))[0];
      return r ? { user_id: String(r.user_id), peer_id: String(r.peer_id) } : null;
    },

    async setTags(userId: string, node: string, tags: string[], mode: "set" | "add" | "remove" = "set"): Promise<string[] | null> {
      await init();
      const r = (await sql.all(`SELECT tags FROM fleet_node_meta WHERE user_id = ? AND node = ?`, [userId, node]))[0];
      if (!r) return null;
      const cur = parseJson<string[]>(r.tags, []);
      const next = mode === "set" ? tags : mode === "add" ? Array.from(new Set([...cur, ...tags])) : cur.filter((t) => !tags.includes(t));
      if (next.length > MAX_TAGS) throw new Error(`at most ${MAX_TAGS} tags per node`);
      await sql.run(`UPDATE fleet_node_meta SET tags = ? WHERE user_id = ? AND node = ?`, [JSON.stringify(next), userId, node]);
      return next;
    },

    async unverifyNode(userId: string, node: string): Promise<boolean> {
      await init();
      const r = await sql.all(`SELECT node FROM fleet_node_meta WHERE user_id = ? AND node = ?`, [userId, node]);
      await sql.run(`DELETE FROM fleet_node_meta WHERE user_id = ? AND node = ?`, [userId, node]);
      return r.length > 0;
    },

    // ── audit ─────────────────────────────────────────────────────────────────
    async audit(userId: string, e: { node?: string | null; action: string; args?: unknown; actor: string; outcome: string }) {
      await init();
      await sql.run(`INSERT INTO fleet_audit (id, user_id, node, action, args, actor, outcome, at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)`, [
        "a_" + randomBytes(10).toString("hex"),
        userId,
        e.node ?? null,
        e.action.slice(0, 64),
        e.args === undefined ? null : JSON.stringify(e.args).slice(0, 2000),
        e.actor.slice(0, 64),
        e.outcome.slice(0, 200),
        now(),
      ]);
    },

    async auditLog(userId: string, limit = 100) {
      await init();
      const rows = await sql.all(`SELECT * FROM fleet_audit WHERE user_id = ? ORDER BY at DESC LIMIT ?`, [userId, Math.min(Math.max(limit, 1), 500)]);
      return rows.map((r) => ({
        id: String(r.id),
        node: (r.node as string) ?? null,
        action: String(r.action),
        args: parseJson<unknown>(r.args, null),
        actor: String(r.actor),
        outcome: String(r.outcome),
        at: String(r.at),
      }));
    },
  };
}

export type AccountStore = ReturnType<typeof createAccountStore>;
