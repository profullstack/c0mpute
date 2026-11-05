// Server-side wiring for c0mpute chat: the store on SQLite Cloud, request auth,
// CoinPay OAuth, the Infernet upstream and a per-user rate limit.
import { Database } from "@sqlitecloud/drivers";
import { createHash, randomBytes } from "node:crypto";
// Read at runtime: Next can inline a literal `process.env.NAME` at build time.
export const env = (name: string): string => (process.env as Record<string, string | undefined>)[name] ?? "";

import { createStore, type Sql, type Store, type User } from "./store";

// ── store ────────────────────────────────────────────────────────────────────
let client: Database | null = null;
function db(): Database {
  const url = env("SQLITE_CLOUD_URL");
  if (!url) throw new Error("SQLITE_CLOUD_URL env var is required");
  return (client ??= new Database(url));
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
    return await db().sql({ query: q, parameters: params as never[] });
  }
}
const cloudSql: Sql = {
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

/** API key (Authorization: Bearer c0k_...) or the browser session cookie. */
export async function currentUser(req: Request): Promise<(User & { token_kind: string }) | null> {
  const token = bearer(req) ?? cookie(req, SESSION_COOKIE);
  try {
    return await store().userForToken(token);
  } catch (e) {
    console.error("chat auth lookup failed", e);
    return null;
  }
}

export function sessionToken(req: Request) {
  return cookie(req, SESSION_COOKIE);
}

/** Cookie-authenticated writes must come from our own pages. */
export function sameOrigin(req: Request): boolean {
  if (bearer(req)) return true;
  const origin = req.headers.get("origin");
  if (!origin) return req.headers.get("sec-fetch-site") === "same-origin";
  return origin === siteUrl() || origin === new URL(req.url).origin;
}

export const siteUrl = () => (env("C0MPUTE_SITE_URL") || "https://c0mpute.com").replace(/\/$/, "");

export function json(body: unknown, status = 200, headers: Record<string, string> = {}) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store", ...headers },
  });
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
 * The person's spendable compute balance, as Infernet reports it.
 * Returns null when it cannot be read: the chat must not break over a balance
 * panel, and an unlinked CoinPay identity simply has no balance yet.
 */
export async function balanceFor(subject: string | null | undefined) {
  if (!subject) return null;
  try {
    const res = await infernet("/api/v1/billing/balance", { signal: AbortSignal.timeout(10_000) }, { subject });
    if (!res.ok) return null;
    return (await res.json()) as Record<string, unknown>;
  } catch {
    return null;
  }
}

// ── per-user rate limit (one container, so memory is the source of truth) ────
const windows = new Map<string, { start: number; n: number }>();
export function rateLimit(userId: string, perHour?: number): { ok: boolean; retryAfter: number } {
  const max = perHour ?? (Number(env("C0MPUTE_CHAT_RPH")) > 0 ? Number(env("C0MPUTE_CHAT_RPH")) : 120);
  const hour = 3_600_000;
  const t = Date.now();
  const w = windows.get(userId);
  if (!w || t - w.start >= hour) {
    windows.set(userId, { start: t, n: 1 });
    return { ok: true, retryAfter: 0 };
  }
  if (w.n >= max) return { ok: false, retryAfter: Math.ceil((w.start + hour - t) / 1000) };
  w.n++;
  return { ok: true, retryAfter: 0 };
}

/** Client IP as nginx on dev2 forwards it. */
export function clientIp(req: Request): string {
  return req.headers.get("x-real-ip") || req.headers.get("x-forwarded-for")?.split(",").pop()?.trim() || "unknown";
}
