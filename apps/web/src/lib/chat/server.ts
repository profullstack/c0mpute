// Server-side wiring for c0mpute chat: the store on SQLite Cloud, request auth,
// CoinPay OAuth, the Infernet upstream and a per-user rate limit.
import { Database } from "@sqlitecloud/drivers";
import { unwrapData } from "./envelope";
import { infernetCall, modelPool, sharedRouter, sqlStatsStore } from "./routed";
import { createHash, randomBytes } from "node:crypto";
// Read at runtime: Next can inline a literal `process.env.NAME` at build time.
export const env = (name: string): string => (process.env as Record<string, string | undefined>)[name] ?? "";

import { createStore, type Sql, type Store, type User } from "./store";
import { createPushStore, type PushStore } from "./push";
import { createOAuthStore, type OAuthStore } from "./oauth";
import { createPluginStore, pluginKey, type PluginStoreDb } from "./plugin-store";
import { convoKey, createConvoStore, type ConvoStore } from "./convo-store";
import { createMediaStore, type MediaKind, type MediaStore } from "./media-owners";

// ── store ────────────────────────────────────────────────────────────────────
let client: Database | null = null;
function db(): Database {
  const url = env("SQLITE_CLOUD_URL");
  if (!url) throw new Error("SQLITE_CLOUD_URL env var is required");
  return (client ??= new Database(url));
}
/**
 * Whether running a statement a second time is harmless. A plain INSERT is
 * not: if the first try reached the database before the socket died, the
 * retry is a duplicate row or a primary-key error for a write that worked.
 */
export function safeToRetry(q: string): boolean {
  const s = q.trimStart().slice(0, 40).toUpperCase();
  if (!s.startsWith("INSERT")) return true;
  return /^INSERT\s+OR\s+(REPLACE|IGNORE)\b/.test(s) || /\bON\s+CONFLICT\b/i.test(q);
}
async function query(q: string, params: unknown[] = []) {
  try {
    return await db().sql({ query: q, parameters: params as never[] });
  } catch (e) {
    // A dropped socket poisons the client; reconnect once.
    try {
      (client as Database & { close?: () => void })?.close?.();
    } catch {}
    client = null;
    if (!safeToRetry(q)) throw e;
    return await db().sql({ query: q, parameters: params as never[] });
  }
}
export const cloudSql: Sql = {
  all: async (q, p) => {
    const r = await query(q, p);
    return Array.isArray(r) ? (r.map((row) => ({ ...row })) as Record<string, unknown>[]) : [];
  },
  run: async (q, p) => {
    await query(q, p);
  },
};

let _store: Store | null = null;
export const store = () => (_store ??= createStore(cloudSql));

let _pushStore: PushStore | null = null;
/** `/push hosting` + `/connect hosting`: linked nodes and pushes, per account. */
export const pushStore = () => (_pushStore ??= createPushStore(cloudSql, { cloudNodeId: env("C0MPUTE_CLOUD_NODE_ID") }));

let _oauthStore: OAuthStore | null = null;
/** OAuth clients and authorization codes for the MCP server (ChatGPT, Claude). */
export const oauthStore = () => (_oauthStore ??= createOAuthStore(cloudSql));

let _convoStore: ConvoStore | null = null;
export const convoStore = () =>
  (_convoStore ??= createConvoStore(cloudSql, { key: convoKey(pluginKey(env("C0MPUTE_CHAT_KEY")), pluginKey(env("C0MPUTE_PLUGIN_KEY"))) }));

/**
 * The model router behind `model: "auto"` (DIP 0035): one per process, its
 * learned stats in SQLite Cloud. Calls default to the unbilled upstream;
 * routedChat passes a per-user one.
 */
export const modelRouter = () =>
  sharedRouter({ pool: modelPool(infernet), store: sqlStatsStore(cloudSql), call: infernetCall(infernet) });

let _mediaStore: MediaStore | null = null;
/**
 * Which account started which image/video/song/album/promo job. Jobs older
 * than this record stay reachable unless C0MPUTE_MEDIA_STRICT_OWNERS=1.
 */
export const mediaStore = () => (_mediaStore ??= createMediaStore(cloudSql, { strict: env("C0MPUTE_MEDIA_STRICT_OWNERS") === "1" }));

// Ids this process already recorded: an album poll names its tracks every few
// seconds, and each needs writing only once.
const recordedJobs = new Set<string>();

/** Remember who started a job. Never fails the request that started it. */
export async function recordJob(userId: string, kind: MediaKind, jobId: unknown): Promise<void> {
  if (typeof jobId !== "string" || recordedJobs.has(jobId)) return;
  try {
    await mediaStore().record(userId, kind, jobId);
    if (recordedJobs.size >= 20_000) recordedJobs.clear();
    recordedJobs.add(jobId);
  } catch (e) {
    console.error(`media owner record failed (${kind})`, e);
  }
}

/** May this account read the job's result? A job recorded for someone else: never. */
export async function ownsJob(userId: string, jobId: string): Promise<boolean> {
  try {
    return await mediaStore().allowed(userId, jobId);
  } catch (e) {
    // The owner could not be read: refuse rather than serve someone else's result.
    console.error("media owner lookup failed", e);
    return false;
  }
}

let _pluginStore: PluginStoreDb | null = null;
/** Chat plugins (remote MCP servers) per account, tokens sealed with C0MPUTE_PLUGIN_KEY. */
export const pluginStore = () => (_pluginStore ??= createPluginStore(cloudSql, { key: pluginKey(env("C0MPUTE_PLUGIN_KEY")) }));

// ── request auth ─────────────────────────────────────────────────────────────
export const SESSION_COOKIE = "c0_session";

export function bearer(req: Request): string | null {
  const h = req.headers.get("authorization") ?? "";
  const m = /^Bearer\s+(\S+)$/i.exec(h);
  return m ? m[1] : null;
}

function cookie(req: Request, name: string): string | null {
  for (const part of (req.headers.get("cookie") ?? "").split(";")) {
    const i = part.indexOf("=");
    if (i > 0 && part.slice(0, i).trim() === name) return decodeURIComponent(part.slice(i + 1).trim());
  }
  return null;
}

/**
 * API key (Authorization: Bearer c0k_...) or the browser session cookie.
 *
 * A key an MCP client got through OAuth (token_kind "oauth") carries the "mcp"
 * scope only: it signs in nowhere but /api/mcp, which passes `{ oauth: true }`.
 * Every other route sees it as signed out, so this is the one place the scope
 * is enforced.
 */
export async function currentUser(req: Request, opts: { oauth?: boolean } = {}): Promise<(User & { token_kind: string }) | null> {
  const token = bearer(req) ?? cookie(req, SESSION_COOKIE);
  try {
    return scopedUser(await store().userForToken(token), opts);
  } catch (e) {
    console.error("chat auth lookup failed", e);
    return null;
  }
}

/** The "mcp" scope: an OAuth-issued key is a user only where `oauth` is allowed. */
export function scopedUser<T extends { token_kind: string }>(user: T | null, opts: { oauth?: boolean } = {}): T | null {
  return user?.token_kind === "oauth" && !opts.oauth ? null : user;
}

export function sessionToken(req: Request) {
  return cookie(req, SESSION_COOKIE);
}

/** Cookie-authenticated writes must come from our own pages. */
export function sameOrigin(req: Request): boolean {
  if (bearer(req)) return true;
  const origin = req.headers.get("origin");
  if (!origin) return req.headers.get("sec-fetch-site") === "same-origin";
  return ownOrigin(req);
}

/** The request's Origin header is this site (a form or fetch from our own pages). */
export function ownOrigin(req: Request): boolean {
  const origin = req.headers.get("origin");
  if (!origin) return false;
  if (origin === siteUrl()) return true;
  // Behind the proxy `req.url` can be an internal origin (http://localhost:3000),
  // which is not ours to trust in production; it is what a local dev server is.
  return env("NODE_ENV") !== "production" && origin === new URL(req.url).origin;
}

export const siteUrl = () => (env("C0MPUTE_SITE_URL") || "https://c0mpute.com").replace(/\/$/, "");

export function json(body: unknown, status = 200, headers: Record<string, string> = {}) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store", ...headers },
  });
}

/**
 * 401 for the MCP server: names the protected-resource metadata, which is how
 * an MCP client (ChatGPT, claude.ai, Claude Code) finds where to sign in.
 */
export function mcpUnauthorized() {
  return json(
    { jsonrpc: "2.0", id: null, error: { code: -32001, message: "Sign in to c0mpute (OAuth), or send Authorization: Bearer c0k_..." } },
    401,
    {
      "www-authenticate": `Bearer realm="c0mpute", resource_metadata="${siteUrl()}/.well-known/oauth-protected-resource"`,
      "access-control-allow-origin": "*",
      "access-control-expose-headers": "www-authenticate",
    },
  );
}

export function unauthorized() {
  return json(
    {
      error: {
        message: "Sign in with CoinPay at https://c0mpute.com/chat (a CoinPay DID is required), or run `c0mpute chat login`.",
        type: "unauthorized",
        code: 401,
      },
    },
    401,
    { "www-authenticate": 'Bearer realm="c0mpute"' },
  );
}

// ── CoinPay OAuth ────────────────────────────────────────────────────────────
export const coinpay = {
  base: () => (env("COINPAY_URL") || "https://coinpayportal.com").replace(/\/$/, ""),
  clientId: () => env("COINPAY_OAUTH_CLIENT_ID") || "",
  clientSecret: () => env("COINPAY_OAUTH_CLIENT_SECRET") || "",
  redirectUri: () => `${siteUrl()}/api/auth/coinpay/callback`,
};

export function pkce() {
  const verifier = randomBytes(48).toString("base64url");
  const challenge = createHash("sha256").update(verifier).digest("base64url");
  return { verifier, challenge, state: randomBytes(18).toString("base64url") };
}

export async function exchangeCode(code: string, verifier: string) {
  const res = await fetch(`${coinpay.base()}/api/oauth/token`, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded", accept: "application/json" },
    body: new URLSearchParams({
      grant_type: "authorization_code",
      code,
      redirect_uri: coinpay.redirectUri(),
      client_id: coinpay.clientId(),
      client_secret: coinpay.clientSecret(),
      code_verifier: verifier,
    }),
    signal: AbortSignal.timeout(15_000),
  });
  const body = (await res.json().catch(() => ({}))) as { access_token?: string; error_description?: string };
  if (!res.ok || !body.access_token) throw new Error(body.error_description || `token exchange HTTP ${res.status}`);
  return body.access_token;
}

export async function coinpayUserinfo(accessToken: string) {
  const res = await fetch(`${coinpay.base()}/api/oauth/userinfo`, {
    headers: { authorization: `Bearer ${accessToken}`, accept: "application/json" },
    signal: AbortSignal.timeout(15_000),
  });
  if (!res.ok) throw new Error(`userinfo HTTP ${res.status}`);
  return (await res.json()) as { sub: string; did?: string; name?: string };
}

// ── Infernet upstream ────────────────────────────────────────────────────────
const infernetBase = () => (env("INFERNET_URL") || "https://infernetprotocol.com").replace(/\/$/, "");

export async function infernet(path: string, init: RequestInit = {}, opts: { subject?: string | null } = {}) {
  const key = env("INFERNET_CLIENT_KEY");
  return fetch(`${infernetBase()}${path}`, {
    ...init,
    headers: {
      ...(init.headers as Record<string, string>),
      ...(key ? { authorization: `Bearer ${key}` } : {}),
      // Name the person this call is for, so Infernet bills their compute
      // balance rather than treating every c0mpute user as one anonymous
      // caller. Infernet resolves it through coinpay_identities on the
      // verified CoinPay sub, and only accepts it from a trusted client key.
      ...(opts.subject ? { "x-infernet-billed-subject": opts.subject } : {}),
    },
  });
}

/**
 * A timeout on the wait for a response's headers only. `AbortSignal.timeout`
 * stays attached to the body, so it cuts a large download (an album zip, a
 * video to a slow client) mid-transfer; call `clear()` once the headers are in.
 */
export function headerTimeout(ms: number) {
  const c = new AbortController();
  const t = setTimeout(() => c.abort(new DOMException("timed out waiting for the network", "TimeoutError")), ms);
  (t as { unref?: () => void }).unref?.();
  return { signal: c.signal, clear: () => clearTimeout(t) };
}

/**
 * An upstream bound to one signed-in person: every call it makes is billed to
 * that person's compute balance (topped up, or what their nodes earned).
 *
 * Pass this instead of `infernet` wherever a request is made on a user's
 * behalf. Without it the request still works, it is just unbilled and served
 * on Infernet's free, rate-limited tier.
 */
export function infernetFor(subject: string | null | undefined) {
  return (path: string, init: RequestInit = {}) => infernet(path, init, { subject: subject ?? null });
}

/**
 * Give this person an account on the network that serves them, so their
 * compute balance can be billed and topped up.
 *
 * Billing on Infernet resolves a c0mpute user through their verified CoinPay
 * sub. That mapping only existed if they had also signed in to
 * infernetprotocol.com with the same CoinPay account, which almost nobody had,
 * so c0mpute users were unbillable and the "top up" link sent them to a site
 * they had no account on. Calling this once at sign-in fixes that.
 *
 * Never throws and never blocks the sign-in: if Infernet is down the person
 * still gets into chat, just on the free tier until the next sign-in links
 * them. Idempotent upstream, so calling it on every sign-in is fine.
 */
export async function linkInfernetAccount(info: { sub?: string; email?: string | null; name?: string | null }) {
  const sub = typeof info?.sub === "string" ? info.sub.trim() : "";
  if (!sub) return null;
  try {
    const res = await infernet("/api/v1/coinpay/link-trusted", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sub, email: info.email ?? null, name: info.name ?? null }),
      signal: AbortSignal.timeout(10_000),
    });
    if (!res.ok) {
      console.warn(`infernet account link failed: HTTP ${res.status}`);
      return null;
    }
    return (await res.json()) as Record<string, unknown>;
  } catch (e) {
    console.warn(`infernet account link failed: ${e instanceof Error ? e.message : e}`);
    return null;
  }
}

/**
 * The person's spendable compute balance, as Infernet reports it.
 * Returns null when it cannot be read: the chat must not break over a balance
 * panel, and an unlinked CoinPay identity simply has no balance yet.
 */
export async function balanceFor(subject: string | null | undefined) {
  if (!subject) return null;
  try {
    const res = await infernet("/api/v1/billing/balance", { signal: AbortSignal.timeout(10_000) }, { subject });
    if (!res.ok) return null;
    // Infernet wraps it as { data: {...} }; every caller reads the fields.
    return unwrapData(await res.json());
  } catch {
    return null;
  }
}

// ── per-user rate limit (one container, so memory is the source of truth) ────
const windows = new Map<string, { start: number; n: number }>();
/** Keys held at once; past it the expired ones go, then the oldest. */
export const RATE_WINDOWS_MAX = 50_000;
let lastSweep = 0;
function sweepWindows(t: number, hour: number) {
  if (windows.size < RATE_WINDOWS_MAX && t - lastSweep < 10 * 60_000) return;
  lastSweep = t;
  for (const [k, w] of windows) if (t - w.start >= hour) windows.delete(k);
  // Still full of live windows (someone rotating addresses): drop the oldest.
  // A Map iterates in insertion order, and a window is re-inserted when it restarts.
  for (const k of windows.keys()) {
    if (windows.size < RATE_WINDOWS_MAX) break;
    windows.delete(k);
  }
}
/** Tests: how many windows are held. */
export const rateWindows = () => windows.size;
export function rateLimit(userId: string, perHour?: number, at = Date.now()): { ok: boolean; retryAfter: number } {
  const max = perHour ?? (Number(env("C0MPUTE_CHAT_RPH")) > 0 ? Number(env("C0MPUTE_CHAT_RPH")) : 120);
  const hour = 3_600_000;
  const t = at;
  sweepWindows(t, hour);
  const w = windows.get(userId);
  if (!w || t - w.start >= hour) {
    windows.delete(userId);
    windows.set(userId, { start: t, n: 1 });
    return { ok: true, retryAfter: 0 };
  }
  if (w.n >= max) return { ok: false, retryAfter: Math.ceil((w.start + hour - t) / 1000) };
  w.n++;
  return { ok: true, retryAfter: 0 };
}

/**
 * Client IP as nginx on dev2 forwards it, as a rate-limit bucket: an IPv6
 * address is cut to its /64, since one subscriber holds the whole /64 and
 * could otherwise use a fresh address per request.
 */
export function clientIp(req: Request): string {
  return ipBucket(req.headers.get("x-real-ip") || req.headers.get("x-forwarded-for")?.split(",").pop()?.trim() || "unknown");
}

export function ipBucket(ip: string): string {
  if (!ip.includes(":") || ip.includes(".")) return ip; // IPv4, IPv4-mapped, or not an address
  const [head, tail = ""] = ip.toLowerCase().split("%")[0].split("::");
  const h = head ? head.split(":") : [];
  const t = tail ? tail.split(":") : [];
  const full = ip.includes("::") ? [...h, ...Array(Math.max(0, 8 - h.length - t.length)).fill("0"), ...t] : h;
  if (full.length !== 8) return ip;
  return `${full.slice(0, 4).map((x) => x.replace(/^0+(?=.)/, "")).join(":")}::/64`;
}
