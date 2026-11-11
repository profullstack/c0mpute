// Chat plugins, per account: remote MCP servers a person added to c0mpute chat
// (not the node plugins under /plugins, which are modules a c0mpute node runs).
//
// One row per (user, slug) in `chat_plugins`, with the server's OAuth tokens
// sealed (AES-256-GCM, key C0MPUTE_PLUGIN_KEY). Dynamic client registrations
// are cached per authorization server in `chat_plugin_clients`, and an OAuth
// sign-in in flight lives in `chat_plugin_pending` keyed by the hash of its
// state, bound to the account that started it.

import { createCipheriv, createDecipheriv, createHash, randomBytes } from "node:crypto";
import type { Row, Sql } from "./store";

export const PLUGIN_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS chat_plugins (
     user_id      TEXT NOT NULL,
     slug         TEXT NOT NULL,
     url          TEXT NOT NULL,
     name         TEXT NOT NULL,
     title        TEXT,
     instructions TEXT,
     auth         TEXT NOT NULL DEFAULT '"none"',
     enabled      INTEGER NOT NULL DEFAULT 1,
     tokens       TEXT,
     expires_at   INTEGER,
     tools        TEXT,
     created_at   TEXT NOT NULL,
     updated_at   TEXT NOT NULL,
     PRIMARY KEY (user_id, slug)
   )`,
  `CREATE TABLE IF NOT EXISTS chat_plugin_clients (
     issuer     TEXT PRIMARY KEY,
     client     TEXT NOT NULL,
     created_at TEXT NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS chat_plugin_pending (
     state_hash TEXT PRIMARY KEY,
     user_id    TEXT NOT NULL,
     slug       TEXT NOT NULL,
     pending    TEXT NOT NULL,
     expires_at TEXT NOT NULL
   )`,
];

export const MAX_PLUGINS = 20;
export const SLUG = /^[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?$/;
/** Slugs that would shadow a static route under /api/v1/chat/plugins/. */
const RESERVED = new Set(["callback", "catalog"]);
const PENDING_MINUTES = 15;

export type PluginAuth =
  | "none"
  | {
      issuer: string;
      authorization_endpoint: string;
      token_endpoint: string;
      registration_endpoint?: string;
      scopes?: string[];
      resource?: string;
      /** false: works without sign-in, signing in adds more. */
      required?: boolean;
    };

/** The package's token record (access + refresh + what refresh needs), sealed as one JSON. */
export type PluginTokens = {
  access_token: string;
  refresh_token?: string;
  expires_at?: number;
  token_type?: string;
  scope?: string;
  client_id?: string;
  client_secret?: string;
  token_endpoint?: string;
  resource?: string;
};

export type ChatPlugin = {
  user_id: string;
  slug: string;
  url: string;
  name: string;
  title: string | null;
  instructions: string | null;
  auth: PluginAuth;
  enabled: boolean;
  /** The token record (opened from the sealed column), or null. */
  tokens: PluginTokens | null;
  /** Shortcut: tokens.access_token. */
  token: string | null;
  /** Unix ms, or null. */
  expires_at: number | null;
  /** Cached tool names + annotations from the last tools/list. */
  tools: { name: string; description?: string; destructive?: boolean }[] | null;
  created_at: string;
  updated_at: string;
};

export class PluginError extends Error {
  constructor(
    message: string,
    public status = 400,
  ) {
    super(message);
  }
}

// ── sealing ─────────────────────────────────────────────────────────────────

/** 32-byte key from base64, base64url or hex. */
export function pluginKey(raw: string | undefined): Buffer | null {
  const s = (raw ?? "").trim();
  if (!s) return null;
  for (const enc of ["hex", "base64url", "base64"] as const) {
    if (enc === "hex" && !/^[0-9a-f]{64}$/i.test(s)) continue;
    const b = Buffer.from(s, enc);
    if (b.length === 32) return b;
  }
  return null;
}

/** v1.<iv>.<tag>.<ciphertext>, all base64url. Additional data binds it to its row. */
export function seal(key: Buffer, plain: string, aad: string): string {
  const iv = randomBytes(12);
  const c = createCipheriv("aes-256-gcm", key, iv);
  c.setAAD(Buffer.from(aad));
  const ct = Buffer.concat([c.update(plain, "utf8"), c.final()]);
  return ["v1", iv.toString("base64url"), c.getAuthTag().toString("base64url"), ct.toString("base64url")].join(".");
}

export function unseal(key: Buffer, sealed: string, aad: string): string | null {
  const [v, iv, tag, ct] = sealed.split(".");
  if (v !== "v1" || !iv || !tag || ct === undefined) return null;
  try {
    // Pin the tag to the full 16 bytes: GCM otherwise accepts a truncated tag,
    // which makes forging a ciphertext far cheaper.
    const t = Buffer.from(tag, "base64url");
    if (t.length !== 16) return null;
    const d = createDecipheriv("aes-256-gcm", key, Buffer.from(iv, "base64url"), { authTagLength: 16 });
    d.setAAD(Buffer.from(aad));
    d.setAuthTag(t);
    return Buffer.concat([d.update(Buffer.from(ct, "base64url")), d.final()]).toString("utf8");
  } catch {
    return null;
  }
}

/** A slug from a server name: a-z0-9 and '-', at most 32. */
export function slugify(name: string): string {
  const s = String(name ?? "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 32)
    .replace(/-+$/g, "");
  return s || "plugin";
}

const now = () => new Date().toISOString();
const sha = (s: string) => createHash("sha256").update(s).digest("hex");

export function createPluginStore(sql: Sql, opts: { key: Buffer | null }) {
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of PLUGIN_SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  const needKey = () => {
    if (!opts.key) throw new PluginError("chat plugins that sign in are not configured on this server (C0MPUTE_PLUGIN_KEY)", 503);
    return opts.key;
  };
  const aad = (userId: string, slug: string, col: string) => `${userId}|${slug}|${col}`;

  function toPlugin(r: Row): ChatPlugin {
    const userId = String(r.user_id);
    const slug = String(r.slug);
    let tokens: PluginTokens | null = null;
    if (typeof r.tokens === "string" && r.tokens && opts.key) {
      const plain = unseal(opts.key, r.tokens, aad(userId, slug, "tokens"));
      try {
        tokens = plain ? JSON.parse(plain) : null;
      } catch {}
    }
    let auth: PluginAuth = "none";
    try {
      auth = JSON.parse(String(r.auth ?? '"none"'));
    } catch {}
    let tools: ChatPlugin["tools"] = null;
    try {
      tools = r.tools ? JSON.parse(String(r.tools)) : null;
    } catch {}
    return {
      user_id: userId,
      slug,
      url: String(r.url),
      name: String(r.name),
      title: (r.title as string) ?? null,
      instructions: (r.instructions as string) ?? null,
      auth,
      enabled: Number(r.enabled) === 1,
      tokens,
      token: tokens?.access_token ?? null,
      expires_at: r.expires_at == null ? null : Number(r.expires_at),
      tools,
      created_at: String(r.created_at),
      updated_at: String(r.updated_at),
    };
  }

  const store = {
    init,

    async list(userId: string): Promise<ChatPlugin[]> {
      await init();
      return (await sql.all(`SELECT * FROM chat_plugins WHERE user_id = ? ORDER BY created_at`, [userId])).map(toPlugin);
    },

    async get(userId: string, slug: string): Promise<ChatPlugin | null> {
      await init();
      const r = (await sql.all(`SELECT * FROM chat_plugins WHERE user_id = ? AND slug = ?`, [userId, slug]))[0];
      return r ? toPlugin(r) : null;
    },

    async byUrl(userId: string, url: string): Promise<ChatPlugin | null> {
      await init();
      const r = (await sql.all(`SELECT * FROM chat_plugins WHERE user_id = ? AND url = ?`, [userId, url]))[0];
      return r ? toPlugin(r) : null;
    },

    /** Add (or refresh the metadata of) a plugin. Re-adding the same URL keeps its slug and tokens. */
    async add(
      userId: string,
      p: { slug?: string; url: string; name: string; title?: string | null; instructions?: string | null; auth: PluginAuth },
    ): Promise<ChatPlugin> {
      await init();
      const existing = await store.byUrl(userId, p.url);
      const t = now();
      const authJson = JSON.stringify(p.auth);
      const title = p.title ? String(p.title).slice(0, 120) : null;
      const instructions = p.instructions ? String(p.instructions).slice(0, 4000) : null;
      if (existing) {
        await sql.run(`UPDATE chat_plugins SET name = ?, title = ?, instructions = ?, auth = ?, updated_at = ? WHERE user_id = ? AND slug = ?`, [
          p.name.slice(0, 120),
          title,
          instructions,
          authJson,
          t,
          userId,
          existing.slug,
        ]);
        return (await store.get(userId, existing.slug))!;
      }
      const all = await store.list(userId);
      if (all.length >= MAX_PLUGINS) throw new PluginError(`at most ${MAX_PLUGINS} chat plugins per account; remove one first`, 409);
      const taken = new Set(all.map((x) => x.slug));
      let base = slugify(p.slug || p.name);
      if (RESERVED.has(base)) base = `${base}-mcp`;
      let slug = base;
      for (let n = 2; taken.has(slug); n++) slug = `${base.slice(0, 28)}-${n}`;
      await sql.run(
        `INSERT INTO chat_plugins (user_id, slug, url, name, title, instructions, auth, enabled, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?)`,
        [userId, slug, p.url, p.name.slice(0, 120), title, instructions, authJson, t, t],
      );
      return (await store.get(userId, slug))!;
    },

    async remove(userId: string, slug: string): Promise<boolean> {
      await init();
      const had = await store.get(userId, slug);
      if (!had) return false;
      await sql.run(`DELETE FROM chat_plugins WHERE user_id = ? AND slug = ?`, [userId, slug]);
      await sql.run(`DELETE FROM chat_plugin_pending WHERE user_id = ? AND slug = ?`, [userId, slug]);
      return true;
    },

    async setEnabled(userId: string, slug: string, enabled: boolean): Promise<ChatPlugin | null> {
      await init();
      await sql.run(`UPDATE chat_plugins SET enabled = ?, updated_at = ? WHERE user_id = ? AND slug = ?`, [enabled ? 1 : 0, now(), userId, slug]);
      return store.get(userId, slug);
    },

    async setTokens(userId: string, slug: string, t: PluginTokens) {
      await init();
      const key = needKey();
      if (!t?.access_token) throw new PluginError("no access token", 502);
      await sql.run(`UPDATE chat_plugins SET tokens = ?, expires_at = ?, enabled = 1, updated_at = ? WHERE user_id = ? AND slug = ?`, [
        seal(key, JSON.stringify(t), aad(userId, slug, "tokens")),
        t.expires_at ?? null,
        now(),
        userId,
        slug,
      ]);
    },

    async clearTokens(userId: string, slug: string) {
      await init();
      await sql.run(`UPDATE chat_plugins SET tokens = NULL, expires_at = NULL, updated_at = ? WHERE user_id = ? AND slug = ?`, [now(), userId, slug]);
    },

    async setTools(userId: string, slug: string, tools: ChatPlugin["tools"]) {
      await init();
      await sql.run(`UPDATE chat_plugins SET tools = ? WHERE user_id = ? AND slug = ?`, [JSON.stringify(tools ?? []).slice(0, 64_000), userId, slug]);
    },

    // ── DCR client cache (per authorization server) ─────────────────────────
    async getClient(issuer: string): Promise<Record<string, unknown> | null> {
      await init();
      const r = (await sql.all(`SELECT client FROM chat_plugin_clients WHERE issuer = ?`, [issuer]))[0];
      if (!r) return null;
      try {
        return JSON.parse(String(r.client));
      } catch {
        return null;
      }
    },
    async setClient(issuer: string, client: Record<string, unknown>) {
      await init();
      await sql.run(`INSERT OR REPLACE INTO chat_plugin_clients (issuer, client, created_at) VALUES (?, ?, ?)`, [issuer, JSON.stringify(client), now()]);
    },
    async deleteClient(issuer: string) {
      await init();
      await sql.run(`DELETE FROM chat_plugin_clients WHERE issuer = ?`, [issuer]);
    },

    // ── OAuth sign-ins in flight ────────────────────────────────────────────
    async savePending(userId: string, slug: string, state: string, pending: unknown) {
      await init();
      const key = needKey();
      const h = sha(state);
      await sql.run(`DELETE FROM chat_plugin_pending WHERE expires_at < ?`, [now()]);
      await sql.run(`INSERT OR REPLACE INTO chat_plugin_pending (state_hash, user_id, slug, pending, expires_at) VALUES (?, ?, ?, ?, ?)`, [
        h,
        userId,
        slug,
        seal(key, JSON.stringify(pending), `pending|${h}|${userId}`),
        new Date(Date.now() + PENDING_MINUTES * 60_000).toISOString(),
      ]);
    },

    /** One-shot: the pending sign-in for this state (deleted as it is read), or null. */
    async takePending(state: string): Promise<{ user_id: string; slug: string; pending: unknown } | null> {
      await init();
      const key = needKey();
      const h = sha(state);
      const r = (await sql.all(`SELECT * FROM chat_plugin_pending WHERE state_hash = ?`, [h]))[0];
      if (!r) return null;
      await sql.run(`DELETE FROM chat_plugin_pending WHERE state_hash = ?`, [h]);
      if (String(r.expires_at) < now()) return null;
      const plain = unseal(key, String(r.pending), `pending|${h}|${String(r.user_id)}`);
      if (!plain) return null;
      return { user_id: String(r.user_id), slug: String(r.slug), pending: JSON.parse(plain) };
    },
  };
  return store;
}

export type PluginStoreDb = ReturnType<typeof createPluginStore>;

/** What an owner sees of a plugin: never a token. */
export function publicPlugin(p: ChatPlugin, site: string) {
  const offersAuth = p.auth !== "none";
  const needsAuth = offersAuth && (p.auth as Exclude<PluginAuth, "none">).required !== false;
  const connected = !needsAuth || !!p.token;
  return {
    slug: p.slug,
    url: p.url,
    name: p.name,
    title: p.title,
    enabled: p.enabled,
    auth: needsAuth ? "oauth" : offersAuth ? "optional" : "none",
    connected,
    signed_in: !!p.token,
    status: !connected ? "needs_sign_in" : p.enabled ? "on" : "off",
    connect_url: offersAuth ? `${site}/api/v1/chat/plugins/${p.slug}/connect` : null,
    tools: (p.tools ?? []).map((t) => ({ name: t.name, description: t.description ?? "", destructive: !!t.destructive })),
    created_at: p.created_at,
  };
}
