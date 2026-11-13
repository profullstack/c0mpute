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
  console.error("chat plugins", e);
  return json({ error: { message: `plugin error: ${e instanceof Error ? e.message : String(e)}`, code: 502 } }, 502);
}

// ── catalog ─────────────────────────────────────────────────────────────────

export type CatalogEntry = { slug: string; name: string; title: string; description: string; url: string };

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
  return cp.CATALOG.filter((e) => e.url.startsWith("https://") && !isOwnHost(e.url)).map((e) => ({
    slug: e.slug,
    name: e.name,
    title: e.name,
    description: e.description ?? "",
    url: e.url,
  }));
}

// ── add ─────────────────────────────────────────────────────────────────────

/** Add a plugin by https URL or catalog name. Re-adding the same URL refreshes it. */
export async function addPlugin(userId: string, target: string, store: PluginStoreDb = pluginStore()): Promise<ChatPlugin> {
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
  const d = await cp.discover(u.href, NET);
  const auth: PluginAuth = d.auth === "none" ? "none" : { ...d.auth };
  const p = await store.add(userId, {
    slug: input.slug,
    url: d.url,
    name: d.name || input.name,
    title: d.title || input.entry?.name || null,
    instructions: d.instructions || null,
    auth,
  });
  if (auth === "none" || auth.required === false) {
    await refreshTools(store, p).catch((e) => console.warn(`chat plugin ${p.slug}: tools/list failed: ${e instanceof Error ? e.message : e}`));
    return (await store.get(userId, p.slug)) ?? p;
  }
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
  const p = await store.get(userId, slug);
  if (!p) throw new PluginError(`no chat plugin ${slug} on this account`, 404);
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
  const fresh = await store.get(got.user_id, got.slug);
  if (fresh) await refreshTools(store, fresh).catch(() => {});
  return { userId: got.user_id, slug: got.slug, name: p.title || p.name };
}

/** The bearer token for this plugin, refreshed (and saved) first when it is about to expire. */
async function tokenFor(store: PluginStoreDb, p: ChatPlugin): Promise<string | undefined> {
  if (!p.tokens) return undefined;
  try {
    const { tokens, refreshed } = await cp.ensureFresh(p.tokens as cp.Tokens, NET);
    if (refreshed) await store.setTokens(p.user_id, p.slug, tokens as PluginTokens);
    return tokens.access_token;
  } catch (e) {
    console.warn(`chat plugin ${p.slug} refresh failed: ${e instanceof Error ? e.message : e}`);
    return p.tokens.access_token;
  }
}

// ── tools ───────────────────────────────────────────────────────────────────

const MAX_TOOLS = 64;

export async function refreshTools(store: PluginStoreDb, p: ChatPlugin) {
  const token = await tokenFor(store, p);
  const tools = await cp.listTools({ url: p.url, ...(token ? { token } : {}) }, { ...NET, maxTools: MAX_TOOLS });
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
export async function livePlugins(userId: string, store: PluginStoreDb, emit: (e: PluginEvent) => void, net: cp.NetOptions = NET): Promise<cp.Plugin[]> {
  const all = (await store.list(userId)).filter(usable);
  const out = await Promise.all(
    all.map(async (p): Promise<cp.Plugin | null> => {
      try {
        const token = await tokenFor(store, p);
        const tools = await cp.listTools({ url: p.url, ...(token ? { token } : {}) }, { ...net, maxTools: MAX_TOOLS });
        return { slug: p.slug, name: p.title || p.name, url: p.url, ...(token ? { token } : {}), enabled: true, tools };
      } catch (e) {
        const reauth = e instanceof cp.PluginError && e.code === "unauthorized";
        emit({ type: "plugin_error", plugin: p.slug, message: reauth ? `${p.slug} needs you to sign in again (/plugin connect ${p.slug})` : `${p.slug} is unavailable: ${e instanceof Error ? e.message : e}` });
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
}): Promise<PluginChatResult> {
  const store = opts.store ?? pluginStore();
  const net = withSignal(opts.net ?? NET, opts.signal);
  const plugins = await livePlugins(opts.userId, store, opts.emit, net);
  let model = opts.req.model ?? "auto";
  if (!plugins.length) {
    const r = await chatText(opts.upstream, { ...opts.req, stream: false }, opts.signal);
    return { text: r.text, model: r.model, stopped: "done" };
  }

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
        opts.emit({ type: "tool_call", id: e.id, plugin: e.plugin, tool: e.tool, arguments: e.arguments });
      } else if (e.type === "tool_result") {
        opts.emit({ type: "tool_result", id: e.id, plugin: e.plugin, tool: e.tool, ok: !e.isError, text: (e.text || "").slice(0, UI_TEXT) });
      } else if (e.type === "approval_needed") {
        const c = e.call;
        opts.emit({ type: "approval_needed", id: c.id, plugin: c.plugin, tool: c.tool, arguments: c.arguments, approval: approvalToken(opts.userId, c.plugin, c.tool, c.arguments) });
      }
    },
  });
  return { text: r.text, model, stopped: r.status === "pending" ? "approval" : "done" };
}
