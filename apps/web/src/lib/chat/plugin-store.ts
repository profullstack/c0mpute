// Chat plugins, per account: remote MCP servers a person added to c0mpute chat
// (not the node plugins under /plugins, which are modules a c0mpute node runs).
//
// One row per (user, slug) in `chat_plugins`, with the server's OAuth tokens
// sealed (AES-256-GCM, key C0MPUTE_PLUGIN_KEY). Dynamic client registrations
// are cached per authorization server in `chat_plugin_clients`, and an OAuth
// sign-in in flight lives in `chat_plugin_pending` keyed by the hash of its
// state, bound to the account that started it.

import { createCipheriv, createDecipheriv, createHash, randomBytes } from "node:crypto";
import { CLAIMS_SCHEMA, claimOnce, type Row, type Sql } from "./store";
import { DOCS_CATALOG, FINANCE_CATALOG, policyFor } from "./plugin-policy";

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
  // Per-plugin settings that outlive a row: "seeded" (a default plugin was
  // added once, so removing it keeps it gone) and "trading_mode" (paper/live).
  `CREATE TABLE IF NOT EXISTS chat_plugin_prefs (
     user_id    TEXT NOT NULL,
     slug       TEXT NOT NULL,
     key        TEXT NOT NULL,
     value      TEXT NOT NULL,
     updated_at TEXT NOT NULL,
     PRIMARY KEY (user_id, slug, key)
   )`,
  // Summaries the model wrote with context__collapse (lib/chat/context-tools.ts),
  // sealed. range = "<from tag>..<to tag>"; kept 30 days.
  `CREATE TABLE IF NOT EXISTS chat_context_collapses (
     user_id    TEXT NOT NULL,
     range      TEXT NOT NULL,
     summary    TEXT NOT NULL,
     created_at TEXT NOT NULL,
     PRIMARY KEY (user_id, range)
   )`,
  CLAIMS_SCHEMA,
];

const COLLAPSE_DAYS = 30;
const COLLAPSES_LOADED = 200;

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
      /** A default plugin added without a network call: discovery runs at the first sign-in. */
      deferred?: boolean;
    };

/** A catalog plugin every account starts with: listed and on, never signed in for them. */
/** `auth: "none"`: the server needs no sign-in, so the seeded row is ready to use. */
export type DefaultPlugin = { slug: string; name: string; url: string; auth?: "none" };

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
  /** A static API key the owner pasted (sent as Authorization: Bearer), not an OAuth sign-in. */
  api_key?: boolean;
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
  /** Brokerage plugins: which account orders go to. New connections start on paper. */
  trading_mode: "paper" | "live";
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

export function createPluginStore(sql: Sql, opts: { key: Buffer | null; defaults?: readonly DefaultPlugin[] }) {
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

  function toPlugin(r: Row, prefs: Map<string, string> = new Map()): ChatPlugin {
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
      trading_mode: prefs.get(`${slug}|trading_mode`) === "live" ? "live" : "paper",
      created_at: String(r.created_at),
      updated_at: String(r.updated_at),
    };
  }

  async function prefsOf(userId: string): Promise<Map<string, string>> {
    const rows = await sql.all(`SELECT slug, key, value FROM chat_plugin_prefs WHERE user_id = ?`, [userId]);
    return new Map(rows.map((r) => [`${String(r.slug)}|${String(r.key)}`, String(r.value)]));
  }

  /**
   * Default plugins, once per account: a row that is on and not signed in
   * (auth discovered at the first sign-in, so this makes no network call),
   * or on and ready to use for a server that needs no sign-in (auth "none").
   * Marked "seeded" so a person who removes one does not get it back, and
   * skipped when the account already has that server or is at the limit.
   */
  const seededHere = new Set<string>();
  async function seedDefaults(userId: string) {
    const defaults = opts.defaults ?? [];
    if (!defaults.length || seededHere.has(userId)) return;
    const prefs = await prefsOf(userId);
    const todo = defaults.filter((d) => !prefs.has(`${d.slug}|seeded`));
    if (todo.length) {
      const rows = await sql.all(`SELECT slug, url FROM chat_plugins WHERE user_id = ?`, [userId]);
      const urls = new Set(rows.map((r) => String(r.url)));
      const slugs = new Set(rows.map((r) => String(r.slug)));
      let count = rows.length;
      const t = now();
      for (const d of todo) {
        if (!urls.has(d.url) && !slugs.has(d.slug) && count < MAX_PLUGINS) {
          const auth: PluginAuth = d.auth === "none" ? "none" : { issuer: "", authorization_endpoint: "", token_endpoint: "", required: true, deferred: true };
          await sql.run(
            `INSERT OR IGNORE INTO chat_plugins (user_id, slug, url, name, title, instructions, auth, enabled, created_at, updated_at) VALUES (?, ?, ?, ?, ?, NULL, ?, 1, ?, ?)`,
            [userId, d.slug, d.url, d.name.slice(0, 120), d.name.slice(0, 120), JSON.stringify(auth), t, t],
          );
          count++;
        }
        await sql.run(`INSERT OR REPLACE INTO chat_plugin_prefs (user_id, slug, key, value, updated_at) VALUES (?, ?, 'seeded', '1', ?)`, [userId, d.slug, t]);
      }
    }
    seededHere.add(userId);
    if (seededHere.size > 50_000) seededHere.clear();
  }

  const store = {
    init,

    async list(userId: string): Promise<ChatPlugin[]> {
      await init();
      await seedDefaults(userId);
      const prefs = await prefsOf(userId);
      return (await sql.all(`SELECT * FROM chat_plugins WHERE user_id = ? ORDER BY created_at`, [userId])).map((r) => toPlugin(r, prefs));
    },

    async get(userId: string, slug: string): Promise<ChatPlugin | null> {
      await init();
      await seedDefaults(userId);
      const r = (await sql.all(`SELECT * FROM chat_plugins WHERE user_id = ? AND slug = ?`, [userId, slug]))[0];
      return r ? toPlugin(r, await prefsOf(userId)) : null;
    },

    async byUrl(userId: string, url: string): Promise<ChatPlugin | null> {
      await init();
      const r = (await sql.all(`SELECT * FROM chat_plugins WHERE user_id = ? AND url = ?`, [userId, url]))[0];
      return r ? toPlugin(r, await prefsOf(userId)) : null;
    },

    /** Brokerage plugins: "paper" (default) or "live". */
    async setTradingMode(userId: string, slug: string, mode: "paper" | "live"): Promise<ChatPlugin | null> {
      await init();
      await sql.run(`INSERT OR REPLACE INTO chat_plugin_prefs (user_id, slug, key, value, updated_at) VALUES (?, ?, 'trading_mode', ?, ?)`, [
        userId,
        slug,
        mode === "live" ? "live" : "paper",
        now(),
      ]);
      return store.get(userId, slug);
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
      // Settings go with it (a re-add starts on paper again); "seeded" stays so a default does not come back.
      await sql.run(`DELETE FROM chat_plugin_prefs WHERE user_id = ? AND slug = ? AND key <> 'seeded'`, [userId, slug]);
      return true;
    },

    async setEnabled(userId: string, slug: string, enabled: boolean): Promise<ChatPlugin | null> {
      await init();
      await sql.run(`UPDATE chat_plugins SET enabled = ?, updated_at = ? WHERE user_id = ? AND slug = ?`, [enabled ? 1 : 0, now(), userId, slug]);
      // Switching it back on is a fresh start for the auto-disable brake.
      if (enabled) await sql.run(`DELETE FROM chat_plugin_prefs WHERE user_id = ? AND slug = ? AND key = 'failures'`, [userId, slug]);
      return store.get(userId, slug);
    },

    /** Rename (the label shown in lists); null or empty goes back to the server's own name. */
    async setTitle(userId: string, slug: string, title: string | null): Promise<ChatPlugin | null> {
      await init();
      const t = title ? String(title).trim().slice(0, 120) : "";
      await sql.run(`UPDATE chat_plugins SET title = ?, updated_at = ? WHERE user_id = ? AND slug = ?`, [t || null, now(), userId, slug]);
      return store.get(userId, slug);
    },

    /**
     * Consecutive failed calls to a plugin (auto-disable brake): a success resets it to 0, a
     * failure adds 1. Returns the new count. Kept in chat_plugin_prefs so a restart keeps it.
     */
    async noteCall(userId: string, slug: string, ok: boolean): Promise<number> {
      await init();
      if (ok) {
        await sql.run(`DELETE FROM chat_plugin_prefs WHERE user_id = ? AND slug = ? AND key = 'failures'`, [userId, slug]);
        return 0;
      }
      const r = (await sql.all(`SELECT value FROM chat_plugin_prefs WHERE user_id = ? AND slug = ? AND key = 'failures'`, [userId, slug]))[0];
      const n = (Number(r?.value) || 0) + 1;
      await sql.run(`INSERT OR REPLACE INTO chat_plugin_prefs (user_id, slug, key, value, updated_at) VALUES (?, ?, 'failures', ?, ?)`, [userId, slug, String(n), now()]);
      return n;
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

    /** This account's recent context__collapse summaries, oldest first. None without C0MPUTE_PLUGIN_KEY. */
    async contextCollapses(userId: string): Promise<{ from: string; to: string; summary: string }[]> {
      if (!opts.key) return [];
      await init();
      const since = new Date(Date.now() - COLLAPSE_DAYS * 86_400_000).toISOString();
      const rows = await sql.all(
        `SELECT range, summary, created_at FROM chat_context_collapses WHERE user_id = ? AND created_at > ? ORDER BY created_at DESC LIMIT ${COLLAPSES_LOADED}`,
        [userId, since],
      );
      const out: { from: string; to: string; summary: string }[] = [];
      for (const r of rows.reverse()) {
        const [from, to] = String(r.range).split("..");
        const summary = unseal(opts.key, String(r.summary), `collapse|${userId}|${String(r.range)}`);
        if (from && to && summary) out.push({ from, to, summary });
      }
      return out;
    },

    /** Keep a context__collapse summary (sealed); a no-op without C0MPUTE_PLUGIN_KEY. Prunes old ones. */
    async saveContextCollapse(userId: string, c: { from: string; to: string; summary: string }) {
      if (!opts.key) return;
      await init();
      const range = `${c.from}..${c.to}`;
      await sql.run(`INSERT OR REPLACE INTO chat_context_collapses (user_id, range, summary, created_at) VALUES (?, ?, ?, ?)`, [
        userId,
        range,
        seal(opts.key, c.summary, `collapse|${userId}|${range}`),
        now(),
      ]);
      const cutoff = new Date(Date.now() - COLLAPSE_DAYS * 86_400_000).toISOString();
      await sql.run(`DELETE FROM chat_context_collapses WHERE user_id = ? AND created_at < ?`, [userId, cutoff]);
    },

    /** One-shot claim for a sign-in ticket (the connect link): true once per ticket. */
    async claimTicket(ticket: string): Promise<boolean> {
      await init();
      return claimOnce(sql, `plugin_ticket:${sha(ticket)}`);
    },

    /** One-shot: the pending sign-in for this state (deleted as it is read), or null. */
    async takePending(state: string): Promise<{ user_id: string; slug: string; pending: unknown } | null> {
      await init();
      const key = needKey();
      const h = sha(state);
      const r = (await sql.all(`SELECT * FROM chat_plugin_pending WHERE state_hash = ?`, [h]))[0];
      if (!r || !(await claimOnce(sql, `plugin_pending:${h}`))) return null;
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
  const policy = policyFor(p.url);
  const entry = FINANCE_CATALOG.find((e) => e.url === p.url) ?? DOCS_CATALOG.find((e) => e.url === p.url);
  return {
    slug: p.slug,
    url: p.url,
    name: p.name,
    title: p.title,
    icon: entry?.icon ?? null,
    kind: policy?.kind ?? null,
    ...(policy?.kind === "trading" ? { trading_mode: p.trading_mode } : {}),
    enabled: p.enabled,
    auth: p.tokens?.api_key ? "api_key" : needsAuth ? "oauth" : offersAuth ? "optional" : "none",
    connected,
    signed_in: !!p.token,
    /** True when it uses an API key the owner pasted; the key itself is never returned. */
    api_key: !!p.tokens?.api_key,
    status: !connected ? "needs_sign_in" : p.enabled ? "on" : "off",
    connect_url: offersAuth ? `${site}/api/v1/chat/plugins/${p.slug}/connect` : null,
    tools: (p.tools ?? []).map((t) => ({ name: t.name, description: t.description ?? "", destructive: !!t.destructive })),
    created_at: p.created_at,
  };
}
