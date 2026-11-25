// Chat plugins: remote MCP servers whose tools the model can use inside
// c0mpute chat (web /chat, the CLI, the desktop app, the API and our own MCP
// server all go through here). The protocol work (discovery, OAuth DCR + PKCE,
// tools/list, tools/call, the tool loop for models without native tool
// calling) is @profullstack/chat-plugins; this file is the c0mpute side:
// which plugins a person has, the extra network guard, sign-in tickets,
// approvals for destructive tools, and billing every model step to the person.

import { createHmac, timingSafeEqual } from "node:crypto";
import * as cp from "@profullstack/chat-plugins";
import { chatText, type ChatRequest, type Upstream } from "./completions";
import { checkPluginUrl, pluginGuard, PluginUrlError } from "./plugin-net";
import { PluginError, type ChatPlugin, type PluginAuth, type PluginStoreDb, type PluginTokens } from "./plugin-store";
import type { PluginEvent } from "./plugin-shared";
import { applyPolicy, describeAction, DOCS_CATALOG, FINANCE_CATALOG, policyFor, redactLog } from "./plugin-policy";
import { env, json, pluginStore, siteUrl } from "./server";

export const CALLBACK_PATH = "/api/v1/chat/plugins/callback";
export const STATE_COOKIE = "c0_plugin_state";
const TICKET_MINUTES = 10;
const APPROVAL_MINUTES = 15;
const MAX_STEPS = 6;
const UI_TEXT = 12_000;

/** Every request the package makes: its own guard plus ours. */
const NET: cp.NetOptions = { guard: pluginGuard, clientInfo: { name: "c0mpute chat", version: "1.0.0" } };

export function pluginErr(e: unknown) {
  if (e instanceof PluginError) return json({ error: { message: e.message, code: e.status } }, e.status);
  if (e instanceof PluginUrlError) return json({ error: { message: e.message, code: 400 } }, 400);
  if (e instanceof cp.PluginError) {
    const code = e.code;
    const status = ["bad_url", "insecure_url", "private_address", "not_mcp", "no_oauth_metadata", "no_pkce", "no_registration", "resource_mismatch", "bad_state", "expired"].includes(code)
      ? 400
      : 502;
    return json({ error: { message: e.message, code: status, reason: code } }, status);
  }
  console.error("chat plugins", redactLog(e));
  return json({ error: { message: `plugin error: ${e instanceof Error ? e.message : String(e)}`, code: 502 } }, 502);
}

// ── catalog ─────────────────────────────────────────────────────────────────

export type CatalogEntry = { slug: string; name: string; title: string; description: string; url: string; icon?: string; default?: boolean };

function hostOf(u: string): string {
  try {
    return new URL(u).hostname.toLowerCase();
  } catch {
    return "";
  }
}

/** Hosts whose MCP server is c0mpute's own (its tools are native in this chat). */
const isOwnHost = (u: string) => {
  const h = hostOf(u);
  return h === hostOf(siteUrl()) || h === "c0mpute.com" || h === "www.c0mpute.com";
};

/**
 * The package catalog minus c0mpute itself: its entry points at
 * c0mpute.com/api/mcp, whose hosting tools are already native in c0mpute chat.
 */
export function catalogFor(_site = siteUrl()): CatalogEntry[] {
  const money: CatalogEntry[] = [...FINANCE_CATALOG, ...DOCS_CATALOG].map((e) => ({ slug: e.slug, name: e.name, title: e.name, description: e.description, url: e.url, icon: e.icon, default: e.default }));
  const taken = new Set(money.map((e) => e.slug));
  const pkg = cp.CATALOG.filter((e) => e.url.startsWith("https://") && !isOwnHost(e.url) && !taken.has(e.slug)).map((e) => ({
    slug: e.slug,
    name: e.name,
    title: e.name,
    description: e.description ?? "",
    url: e.url,
    // CheapestWayz is on by default (chat-plugins 0.3.1): sign-in on first use, never a shared account.
    ...(e.icon ? { icon: e.icon } : {}),
    ...(e.default ? { default: true } : {}),
  }));
  return [...money, ...pkg];
}

/** A discovered auth record with the money policy's narrower scopes, when it has some. */
function withPolicyScopes(url: string, auth: PluginAuth): PluginAuth {
  const scopes = policyFor(url)?.scopes;
  return auth === "none" || !scopes ? auth : { ...auth, scopes: [...scopes] };
}

// ── add ─────────────────────────────────────────────────────────────────────

/** Add a plugin by https URL or catalog name. Re-adding the same URL refreshes it. */
export type AddOptions = {
  /** A static API key, for servers that take `Authorization: Bearer <key>` instead of (or as well as) OAuth. */
  apiKey?: string | null;
  /** The label to show instead of the server's own name. */
  title?: string | null;
  net?: cp.NetOptions;
};

export async function addPlugin(userId: string, target: string, store: PluginStoreDb = pluginStore(), opts: AddOptions = {}): Promise<ChatPlugin> {
  const net = opts.net ?? NET;
  const catalog = catalogFor();
  const raw = String(target ?? "").trim();
  const looksUrl = /^[a-z][a-z0-9+.-]*:\/\//i.test(raw) || raw.includes(".") || raw.includes("/");
  const input = cp.resolvePluginInput(raw, catalog.map((c) => ({ slug: c.slug, name: c.name, url: c.url, description: c.description })));
  if (!input || (!input.entry && !looksUrl)) {
    const names = catalog.map((c) => c.slug);
    throw new PluginError(`no catalog plugin named "${raw}"${names.length ? ` (catalog: ${names.join(", ")})` : ""}; or pass an https:// MCP server URL`, 404);
  }
  if (isOwnHost(input.url)) throw new PluginError("that is c0mpute's own MCP server: its tools are already built into this chat");
  const u = checkPluginUrl(input.url);
  const apiKey = opts.apiKey ? cleanKey(opts.apiKey) : null;
  let d: Pick<cp.DiscoveredServer, "url" | "name" | "auth"> & { title?: string; instructions?: string };
  try {
    d = await cp.discover(u.href, net);
  } catch (e) {
    // A server that answers 401 and publishes no OAuth metadata takes a key, not a sign-in.
    if (!(e instanceof cp.PluginError && e.code === "no_oauth_metadata")) throw e;
    if (!apiKey) throw new PluginError(`${u.host} needs a key and has no OAuth sign-in: add it with its API key (api_key)`);
    const r = await withKey(u.href, apiKey, (conn) => cp.initialize(conn, net));
    const si = (r.info?.serverInfo ?? {}) as { name?: unknown; title?: unknown };
    d = {
      url: u.href,
      name: typeof si.name === "string" && si.name ? si.name : u.hostname.split(".")[0],
      title: typeof si.title === "string" ? si.title : undefined,
      auth: "none",
    };
  }
  const auth: PluginAuth = withPolicyScopes(d.url, d.auth === "none" ? "none" : { ...d.auth });
  const title = opts.title?.trim() || null;
  let p = await store.add(userId, {
    slug: input.slug,
    url: d.url,
    name: d.name || input.name,
    title: title || d.title || input.entry?.name || null,
    instructions: d.instructions || null,
    auth,
  });
  if (title) p = (await store.setTitle(userId, p.slug, title)) ?? p;
  if (apiKey) return setApiKey(store, p, apiKey, net);
  if (auth === "none" || auth.required === false) {
    await refreshTools(store, p, net).catch((e) => console.warn(`chat plugin ${p.slug}: tools/list failed: ${redactLog(e)}`));
    return (await store.get(userId, p.slug)) ?? p;
  }
  return p;
}

// ── API keys and edits ──────────────────────────────────────────────────────

const KEY_MAX = 4096;

/** One opaque token: a pasted "Bearer " prefix is dropped; whitespace or control characters are refused. */
export function cleanKey(raw: unknown): string {
  const k = String(raw ?? "")
    .trim()
    .replace(/^bearer\s+/i, "");
  if (!k || k.length > KEY_MAX || /[\s\u0000-\u001f\u007f]/.test(k)) throw new PluginError(`api_key must be one token with no spaces, at most ${KEY_MAX} characters`);
  return k;
}

/** Run one MCP call with a key; a 401/403 becomes "that key was refused". */
async function withKey<T>(url: string, key: string, fn: (conn: { url: string; token: string }) => Promise<T>): Promise<T> {
  try {
    return await fn({ url, token: key });
  } catch (e) {
    if (e instanceof cp.PluginError && (e.code === "unauthorized" || e.code === "forbidden")) throw new PluginError(`${hostOf(url)} refused that API key`, 400);
    throw e;
  }
}

/**
 * Check a static API key against the server (tools/list with it as the
 * bearer token), then keep it sealed like an OAuth token. No API ever
 * returns the key.
 */
export async function setApiKey(store: PluginStoreDb, p: ChatPlugin, rawKey: string, net: cp.NetOptions = NET): Promise<ChatPlugin> {
  const key = cleanKey(rawKey);
  await withKey(p.url, key, (conn) => cp.listTools(conn, { ...net, maxTools: MAX_TOOLS }));
  await store.setTokens(p.user_id, p.slug, { access_token: key, token_type: "Bearer", api_key: true });
  const fresh = (await store.get(p.user_id, p.slug)) ?? p;
  await refreshTools(store, fresh, net).catch((e) => console.warn(`chat plugin ${p.slug}: tools/list failed: ${redactLog(e)}`));
  return (await store.get(p.user_id, p.slug)) ?? fresh;
}

export type PluginPatch = {
  /** New label; null or "" goes back to the server's own name. */
  title?: string | null;
  enabled?: boolean;
  /** A new API key, or null to forget the stored key (or OAuth sign-in). */
  api_key?: string | null;
};

/** Read a PATCH body into a PluginPatch, refusing anything else. */
export function readPatch(body: unknown): PluginPatch {
  if (!body || typeof body !== "object" || Array.isArray(body)) throw new PluginError("send a JSON object: {title?, enabled?, api_key?}");
  const b = body as Record<string, unknown>;
  for (const k of Object.keys(b)) if (!["title", "enabled", "api_key"].includes(k)) throw new PluginError(`unknown field "${k}" (title, enabled, api_key)`);
  const out: PluginPatch = {};
  if ("title" in b) {
    if (b.title !== null && typeof b.title !== "string") throw new PluginError("title must be a string or null");
    out.title = b.title as string | null;
  }
  if ("enabled" in b) {
    if (typeof b.enabled !== "boolean") throw new PluginError("enabled must be true or false");
    out.enabled = b.enabled;
  }
  if ("api_key" in b) {
    if (b.api_key !== null && typeof b.api_key !== "string") throw new PluginError("api_key must be a string, or null to remove it");
    out.api_key = b.api_key === null ? null : cleanKey(b.api_key);
  }
  if (!Object.keys(out).length) throw new PluginError("nothing to change: send title, enabled or api_key");
  return out;
}

/** Edit a plugin: rename it, switch it on/off, set or clear its API key. */
export async function updatePlugin(userId: string, slug: string, patch: PluginPatch, store: PluginStoreDb = pluginStore(), net: cp.NetOptions = NET): Promise<ChatPlugin> {
  let p = await store.get(userId, slug);
  if (!p) throw new PluginError(`no chat plugin ${slug} on this account`, 404);
  // The key first: a refused key changes nothing else.
  if (typeof patch.api_key === "string") p = await setApiKey(store, p, patch.api_key, net);
  else if (patch.api_key === null) {
    await store.clearTokens(userId, slug);
    p = (await store.get(userId, slug)) ?? p;
  }
  if ("title" in patch) p = (await store.setTitle(userId, slug, patch.title ?? null)) ?? p;
  if (typeof patch.enabled === "boolean") p = (await store.setEnabled(userId, slug, patch.enabled)) ?? p;
  return p;
}

// ── sign-in tickets (one-time links for the CLI, API, MCP and desktop) ─────

function hmacKey(purpose: string): Buffer {
  const raw = env("C0MPUTE_PLUGIN_KEY");
  if (!raw) throw new PluginError("plugin sign-in is not configured on this server (C0MPUTE_PLUGIN_KEY)", 503);
  return createHmac("sha256", raw).update(`chat-plugin-${purpose}`).digest();
}

function signed(purpose: string, payload: object): string {
  const body = Buffer.from(JSON.stringify(payload)).toString("base64url");
  return `${body}.${createHmac("sha256", hmacKey(purpose)).update(body).digest("base64url")}`;
}

function verified<T>(purpose: string, token: unknown): T | null {
  if (typeof token !== "string" || token.length > 20_000) return null;
  const [body, sig] = token.split(".");
  if (!body || !sig) return null;
  let want: Buffer;
  try {
    want = createHmac("sha256", hmacKey(purpose)).update(body).digest();
  } catch {
    return null;
  }
  const got = Buffer.from(sig, "base64url");
  if (got.length !== want.length || !timingSafeEqual(got, want)) return null;
  try {
    return JSON.parse(Buffer.from(body, "base64url").toString()) as T;
  } catch {
    return null;
  }
}

export function makeTicket(userId: string, slug: string, at = Date.now()): string {
  return signed("ticket", { u: userId, s: slug, e: Math.floor(at / 1000) + TICKET_MINUTES * 60 });
}

/** The user id a ticket was issued to, if it is genuine, unexpired and for this slug. */
export function verifyTicket(ticket: string, slug: string, at = Date.now()): string | null {
  const t = verified<{ u: string; s: string; e: number }>("ticket", ticket);
  return t && t.s === slug && t.e * 1000 >= at ? t.u : null;
}

/**
 * A ticket link names the account that will receive the plugin's tokens. If
 * this browser is signed in to c0mpute as someone else, the link was made by
 * another account: connecting would hand that account this person's sign-in
 * at the provider, so it is refused.
 */
export function ticketForOtherAccount(ticketUserId: string, sessionUserId: string | null | undefined): boolean {
  return !!sessionUserId && sessionUserId !== ticketUserId;
}

/** Use a ticket up: true once, false for every later try (and for a bad ticket). */
export async function consumeTicket(ticket: string, slug: string, store: Pick<PluginStoreDb, "claimTicket"> = pluginStore()): Promise<string | null> {
  const userId = verifyTicket(ticket, slug);
  if (!userId) return null;
  return (await store.claimTicket(ticket)) ? userId : null;
}

export const connectLink = (userId: string, slug: string) =>
  `${siteUrl()}/api/v1/chat/plugins/${encodeURIComponent(slug)}/connect?t=${makeTicket(userId, slug)}`;

// ── OAuth ───────────────────────────────────────────────────────────────────

const redirectUri = () => `${siteUrl()}${CALLBACK_PATH}`;

/** The package's PluginStore (registered clients), backed by chat_plugin_clients. */
function pkgStore(store: PluginStoreDb): cp.PluginStore {
  return {
    getClient: async (key) => (await store.getClient(key)) as cp.ClientRecord | null,
    setClient: (key, client) => store.setClient(key, client),
    deleteClient: (key) => store.deleteClient(key),
  };
}

export async function startConnect(userId: string, slug: string, store: PluginStoreDb = pluginStore()) {
  let p = await store.get(userId, slug);
  if (!p) throw new PluginError(`no chat plugin ${slug} on this account`, 404);
  if (p.auth !== "none" && (p.auth.deferred || !p.auth.authorization_endpoint)) {
    // A default plugin: discover its sign-in now, the first time the person uses it.
    const d = await cp.discover(checkPluginUrl(p.url).href, NET);
    p = await store.add(userId, {
      url: p.url,
      name: d.name || p.name,
      title: p.title || d.title || null,
      instructions: d.instructions || null,
      auth: withPolicyScopes(p.url, d.auth === "none" ? "none" : { ...d.auth }),
    });
  }
  if (p.auth === "none") throw new PluginError(`${slug} needs no sign-in`);
  const { authorizeUrl, pending } = await cp.startAuth({
    ...NET,
    server: { url: p.url, name: p.name, title: p.title ?? p.name, instructions: p.instructions ?? "", auth: p.auth as cp.AuthInfo },
    redirectUri: redirectUri(),
    clientName: "c0mpute chat",
    clientUri: siteUrl(),
    store: pkgStore(store),
  });
  // The browser goes there, not us: https only, and never a private host.
  const au = checkPluginUrl(authorizeUrl);
  await store.savePending(userId, slug, pending.state, pending);
  return { authorizeUrl: au.href, state: pending.state };
}

export async function finishConnect(state: string, code: string, store: PluginStoreDb = pluginStore()) {
  const got = await store.takePending(state);
  if (!got) throw new PluginError("this sign-in expired or was already used; start it again");
  const p = await store.get(got.user_id, got.slug);
  if (!p) throw new PluginError("that plugin was removed");
  const tokens = await cp.finishAuth({ ...NET, pending: got.pending as cp.PendingAuth, code, state, redirectUri: redirectUri() });
  await store.setTokens(got.user_id, got.slug, tokens as PluginTokens);
  // Every new brokerage connection starts on paper; live is the person's own switch.
  if (policyFor(p.url)?.kind === "trading") await store.setTradingMode(got.user_id, got.slug, "paper");
  const fresh = await store.get(got.user_id, got.slug);
  if (fresh) await refreshTools(store, fresh).catch(() => {});
  return { userId: got.user_id, slug: got.slug, name: p.title || p.name };
}

/** The bearer token for this plugin, refreshed (and saved) first when it is about to expire. */
async function tokenFor(store: PluginStoreDb, p: ChatPlugin): Promise<string | undefined> {
  if (!p.tokens) return undefined;
  // A pasted API key has no refresh flow.
  if (p.tokens.api_key) return p.tokens.access_token;
  try {
    const { tokens, refreshed } = await cp.ensureFresh(p.tokens as cp.Tokens, NET);
    if (refreshed) await store.setTokens(p.user_id, p.slug, tokens as PluginTokens);
    return tokens.access_token;
  } catch (e) {
    console.warn(`chat plugin ${p.slug} refresh failed: ${redactLog(e)}`);
    return p.tokens.access_token;
  }
}

// ── tools ───────────────────────────────────────────────────────────────────

const MAX_TOOLS = 64;

export async function refreshTools(store: PluginStoreDb, p: ChatPlugin, net: cp.NetOptions = NET) {
  const token = await tokenFor(store, p);
  const tools = applyPolicy(policyFor(p.url), await cp.listTools({ url: p.url, ...(token ? { token } : {}) }, { ...net, maxTools: MAX_TOOLS }), p.trading_mode);
  await store.setTools(
    p.user_id,
    p.slug,
    tools.map((t) => ({ name: t.name, description: (t.description ?? "").slice(0, 300), destructive: cp.needsApproval(t) })),
  );
  return tools;
}

/** A plugin can be used when it is on and either needs no sign-in or has a token. */
const usable = (p: ChatPlugin) => p.enabled && (p.auth === "none" || p.auth.required === false || !!p.token);

/** Enabled, usable plugins with their live tool lists, as the loop takes them. Failures become events. */
export async function livePlugins(
  userId: string,
  store: PluginStoreDb,
  emit: (e: PluginEvent) => void,
  net: cp.NetOptions = NET,
  policy: typeof policyFor = policyFor,
): Promise<cp.Plugin[]> {
  const all = (await store.list(userId)).filter(usable);
  const out = await Promise.all(
    all.map(async (p): Promise<cp.Plugin | null> => {
      try {
        const token = await tokenFor(store, p);
        // Money plugins: only allowlisted reads run on their own; everything else asks (plugin-policy.ts).
        const tools = applyPolicy(policy(p.url), await cp.listTools({ url: p.url, ...(token ? { token } : {}) }, { ...net, maxTools: MAX_TOOLS }), p.trading_mode);
        return { slug: p.slug, name: p.title || p.name, url: p.url, ...(token ? { token } : {}), enabled: true, tools };
      } catch (e) {
        const reauth = e instanceof cp.PluginError && e.code === "unauthorized";
        emit({ type: "plugin_error", plugin: p.slug, message: reauth ? `${p.slug} needs you to sign in again (/plugin connect ${p.slug})` : `${p.slug} is unavailable: ${e instanceof Error ? e.message : e}` });
        await noteOutcome(store, userId, p.slug, false, emit);
        return null;
      }
    }),
  );
  return out.filter((x): x is cp.Plugin => !!x && x.tools.length > 0);
}

/** Does this account have any usable plugin on? (no network) */
export async function hasEnabledPlugins(userId: string, store: PluginStoreDb = pluginStore()): Promise<boolean> {
  try {
    return (await store.list(userId)).some(usable);
  } catch {
    return false;
  }
}

// ── approvals ───────────────────────────────────────────────────────────────

/** A signed approval for one exact call (plugin, tool, arguments), bound to the account. */
export function approvalToken(userId: string, slug: string, tool: string, args: unknown, at = Date.now()): string {
  return signed("approve", { u: userId, p: slug, t: tool, a: args ?? {}, e: Math.floor(at / 1000) + APPROVAL_MINUTES * 60 });
}

export function readApproval(userId: string, token: unknown, at = Date.now()): { slug: string; tool: string; args: Record<string, unknown> } | null {
  const t = verified<{ u: string; p: string; t: string; a: Record<string, unknown>; e: number }>("approve", token);
  if (!t || t.u !== userId || t.e * 1000 < at) return null;
  return { slug: t.p, tool: t.t, args: t.a && typeof t.a === "object" ? t.a : {} };
}

// ── the tool loop ───────────────────────────────────────────────────────────

export type PluginChatResult = { text: string; model: string; stopped: "done" | "approval" };

/**
 * One chat turn with the person's plugins, through the package's tool loop in
 * prompt mode (Infernet's chat has no native tool calling). Every model step
 * is a normal billed completion on `upstream`. A destructive call stops the
 * turn with an `approval_needed` event carrying a signed token; sending the
 * same messages back with `approvals: [token]` resumes the loop by running
 * exactly that call first.
 */
/**
 * Thread an interrupt into every plugin request (tool calls, tools/list): the
 * tool loop has no signal of its own, so a Stop used to wait for the running
 * tool (up to 60 s) and only took effect at the next model step.
 */
export function withSignal(net: cp.NetOptions, signal?: AbortSignal): cp.NetOptions {
  if (!signal) return net;
  const base = net.fetch ?? fetch;
  const wrapped = ((input: Parameters<typeof fetch>[0], init?: RequestInit) => {
    const s = init?.signal ? AbortSignal.any([init.signal, signal]) : signal;
    return base(input, { ...init, signal: s });
  }) as typeof fetch;
  return { ...net, fetch: wrapped };
}

// ── brakes (board meeting 2026-11-25: caps, auto-disable, capacity throttle) ──────

const HOUR = 3_600_000;
const DAY = 24 * HOUR;
const intEnv = (k: string, d: number) => {
  const n = Number(env(k));
  return Number.isFinite(n) && n > 0 ? Math.floor(n) : d;
};
/** Consecutive failed calls before a plugin is switched off for the account. */
export const failLimit = () => intEnv("C0MPUTE_PLUGIN_FAIL_LIMIT", 5);
const stepsPerHour = () => intEnv("C0MPUTE_PLUGIN_STEPS_PER_HOUR", 120);
const stepsPerDay = () => intEnv("C0MPUTE_PLUGIN_STEPS_PER_DAY", 600);
/** Plugin turns running at once on this server; more get a "busy, try again" answer, not a queue that grows. */
const maxTurns = () => intEnv("C0MPUTE_PLUGIN_CONCURRENCY", 12);

type StepWindow = { start: number; n: number };
const stepWindows = new Map<string, StepWindow>();
let activeTurns = 0;

function stepWindow(key: string, span: number, at: number): StepWindow {
  const w = stepWindows.get(key);
  return !w || at - w.start >= span ? { start: at, n: 0 } : w;
}

/** Can this account start another tool-using turn? (per-hour and per-day tool-step caps) */
export function stepBudget(userId: string, at = Date.now()): { ok: true } | { ok: false; retryAfter: number; message: string } {
  for (const [suffix, span, max, label] of [
    ["h", HOUR, stepsPerHour(), "an hour"],
    ["d", DAY, stepsPerDay(), "a day"],
  ] as const) {
    const w = stepWindow(`${userId}:${suffix}`, span, at);
    if (w.n >= max) {
      const retryAfter = Math.ceil((w.start + span - at) / 1000);
      return { ok: false, retryAfter, message: `you have used this account's ${max} plugin tool steps ${label}; plugins work again in ${Math.ceil(retryAfter / 60)} min (plain chat still works: plugins: false or /plugin off)` };
    }
  }
  return { ok: true };
}

/** Count one tool call against the account's caps. */
export function countStep(userId: string, at = Date.now()) {
  if (stepWindows.size > 50_000) for (const [k, w] of stepWindows) if (at - w.start >= DAY) stepWindows.delete(k);
  for (const [suffix, span] of [
    ["h", HOUR],
    ["d", DAY],
  ] as const) {
    const key = `${userId}:${suffix}`;
    const w = stepWindow(key, span, at);
    w.n++;
    stepWindows.set(key, w);
  }
}

/** Tests only. */
export function resetBrakes() {
  stepWindows.clear();
  activeTurns = 0;
}

/**
 * Record one call's outcome; after failLimit() failures in a row the plugin is switched off for
 * this account and the person is told how to turn it back on.
 */
async function noteOutcome(store: PluginStoreDb, userId: string, slug: string, ok: boolean, emit: (e: PluginEvent) => void) {
  try {
    const n = await store.noteCall(userId, slug, ok);
    if (!ok && n >= failLimit()) {
      await store.setEnabled(userId, slug, false);
      await store.noteCall(userId, slug, true);
      emit({ type: "plugin_error", plugin: slug, message: `${slug} was switched off after ${n} failed calls in a row; fix it (/plugin key or /plugin connect) and turn it back on with /plugin on ${slug}` });
    }
  } catch (e) {
    console.warn(`chat plugin ${slug}: failure count not saved: ${redactLog(e)}`);
  }
}

export async function runPluginChat(opts: {
  userId: string;
  upstream: Upstream;
  req: ChatRequest;
  approvals?: unknown[];
  emit: (e: PluginEvent) => void;
  store?: PluginStoreDb;
  signal?: AbortSignal;
  /** Tests only: network options for a local MCP server. */
  net?: cp.NetOptions;
  /** Tests only: the money policy for a local URL. */
  policy?: typeof policyFor;
}): Promise<PluginChatResult> {
  const store = opts.store ?? pluginStore();
  const net = withSignal(opts.net ?? NET, opts.signal);
  const policy = opts.policy ?? policyFor;
  const plugins = await livePlugins(opts.userId, store, opts.emit, net, policy);
  if (!plugins.length) {
    const r = await chatText(opts.upstream, { ...opts.req, stream: false }, opts.signal);
    return { text: r.text, model: r.model, stopped: "done" };
  }
  const budget = stepBudget(opts.userId);
  if (!budget.ok) throw new PluginError(budget.message, 429);
  if (activeTurns >= maxTurns())
    throw new PluginError("tool-using chats on c0mpute are at capacity right now; nothing was charged, try again in a minute (plain chat still works: plugins: false)", 503);
  activeTurns++;
  try {
    return await pluginTurn(opts, store, net, policy, plugins);
  } finally {
    activeTurns--;
  }
}

async function pluginTurn(
  opts: Parameters<typeof runPluginChat>[0],
  store: PluginStoreDb,
  net: cp.NetOptions,
  policy: typeof policyFor,
  plugins: cp.Plugin[],
): Promise<PluginChatResult> {
  let model = opts.req.model ?? "auto";
  const outcomes: Promise<void>[] = [];
  const complete = async (msgs: { role: string; content: string }[]) => {
    if (opts.signal?.aborted) throw new Error("cancelled");
    const messages = msgs
      .filter((m) => m.role === "system" || m.role === "user" || m.role === "assistant")
      .map((m) => ({ role: m.role as "system" | "user" | "assistant", content: typeof m.content === "string" ? m.content : "" }));
    const r = await chatText(opts.upstream, { ...opts.req, messages, stream: false }, opts.signal);
    model = r.model;
    return r.text;
  };

  // A call the person approved: resume as if the model had just asked for it.
  let messages: { role: string; content: string }[] = opts.req.messages.slice();
  let resume: cp.ToolLoopOptions["resume"];
  const approved = (opts.approvals ?? []).map((t) => readApproval(opts.userId, t)).find(Boolean);
  if (approved) {
    const plugin = plugins.find((p) => p.slug === approved.slug);
    const tool = plugin?.tools.find((t) => t.name === approved.tool);
    if (plugin && tool) {
      const name = cp.toolName(plugin.slug, tool.name);
      messages = [...messages, { role: "assistant", content: "```tool_call\n" + JSON.stringify({ name, arguments: approved.args }) + "\n```" }];
      const call: cp.ApprovalCall = {
        id: `call_${Date.now().toString(36)}`,
        name,
        plugin: plugin.slug,
        pluginName: plugin.name ?? plugin.slug,
        tool: tool.name,
        title: tool.title || tool.name,
        arguments: approved.args,
        annotations: tool.annotations ?? {},
        kind: "prompt",
      };
      countStep(opts.userId);
      opts.emit({ type: "tool_call", id: call.id, plugin: call.plugin, tool: call.tool, arguments: call.arguments });
      resume = { pending: { call, remaining: [], step: 1, messages: [] }, decision: "allow" };
    }
  }

  const r = await cp.runToolLoop({
    messages,
    plugins,
    complete,
    protocol: "prompt",
    maxSteps: MAX_STEPS,
    net: { ...net, timeoutMs: 60_000 },
    approve: () => "pending",
    ...(resume ? { resume } : {}),
    onEvent: (e) => {
      if (e.type === "tool_call") {
        countStep(opts.userId);
        opts.emit({ type: "tool_call", id: e.id, plugin: e.plugin, tool: e.tool, arguments: e.arguments });
      } else if (e.type === "tool_result") {
        opts.emit({ type: "tool_result", id: e.id, plugin: e.plugin, tool: e.tool, ok: !e.isError, text: (e.text || "").slice(0, UI_TEXT) });
        outcomes.push(noteOutcome(store, opts.userId, e.plugin, !e.isError, opts.emit));
      } else if (e.type === "approval_needed") {
        const c = e.call;
        const url = plugins.find((p) => p.slug === c.plugin)?.url ?? "";
        const summary = describeAction(policy(url), c.tool, c.arguments ?? {});
        opts.emit({
          type: "approval_needed",
          id: c.id,
          plugin: c.plugin,
          tool: c.tool,
          arguments: c.arguments,
          approval: approvalToken(opts.userId, c.plugin, c.tool, c.arguments),
          ...(summary ? { summary } : {}),
        });
      }
    },
  });
  await Promise.all(outcomes);
  return { text: r.text, model, stopped: r.status === "pending" ? "approval" : "done" };
}
