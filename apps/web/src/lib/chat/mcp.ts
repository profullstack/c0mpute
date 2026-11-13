// MCP (JSON-RPC 2.0, Streamable HTTP with plain JSON responses) for c0mpute chat.
// Tools: chat, list_models, whoami, promo_video, promo_status. Auth is the same
// c0k_ API key as the REST API.
import { chatText, listModels, parseChatRequest, type ChatMessage, type ChatRequest, type Upstream } from "./completions";
import { describePromoProgress, sanitizePromoProgress, PROMO_STYLES } from "./promo";
import { parsePublicPromoRequest } from "./promo-guard";
import { HOSTING_TOOL_NAMES, HOSTING_TOOLS, hostingTool } from "./push-mcp";
import { CONVO_ID, type ConvoStore } from "./convo-store";
import type { PushStore } from "./push";
import { describeSteps, PLUGIN_TOOL_NAMES, PLUGIN_TOOLS, pluginTool, type PluginMcpDeps } from "./plugin-mcp";
import type { PluginEvent } from "./plugin-shared";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const site = () => (process.env.C0MPUTE_SITE_URL || "https://c0mpute.com").replace(/\/$/, "");

export const PROTOCOL_VERSION = "2025-06-18";

export const TOOLS = [
  {
    name: "chat",
    description:
      "Ask a model running on the c0mpute network (Infernet provider nodes). Returns the answer text. Pass `messages` for a multi-turn conversation, or just `prompt`.",
    inputSchema: {
      type: "object",
      properties: {
        prompt: { type: "string", description: "The user message." },
        system: { type: "string", description: "Optional system prompt." },
        messages: {
          type: "array",
          description: "Full conversation instead of prompt: [{role, content}].",
          items: {
            type: "object",
            properties: { role: { type: "string", enum: ["system", "user", "assistant"] }, content: { type: "string" } },
            required: ["role", "content"],
          },
        },
        model: { type: "string", description: "Model id from list_models; omit for any live model." },
        max_tokens: { type: "number" },
        temperature: { type: "number" },
        plugins: { type: "boolean", description: "Use this account's chat plugins (default true when any is on; see chat_plugins_list)." },
        conversation: {
          type: "string",
          description:
            "Optional: continue a conversation synced to this account (the same history the c0mpute.com chat, TUI and CLI see; ids from chat_conversations). Its messages come first and this turn is saved to it. A new id starts one.",
        },
      },
    },
    annotations: { title: "Ask a network model", readOnlyHint: true, openWorldHint: true },
  },
  {
    name: "list_models",
    description: "Models live on the c0mpute network right now.",
    inputSchema: { type: "object", properties: {} },
    annotations: { title: "List models", readOnlyHint: true, openWorldHint: false },
  },
  {
    name: "promo_video",
    description:
      "Start a 20-40 second cartoon promo video (MP4 with narration, background music and captions) for a website, made on the c0mpute network: it reads the site's branding, writes a storyboard, draws keyframes and cuts them together. Takes 5 to 15 minutes; returns a job id. Call promo_status with it until it is done.",
    inputSchema: {
      type: "object",
      properties: {
        url: { type: "string", description: "The website to promote (public http/https)." },
        style: { type: "string", enum: PROMO_STYLES, description: "Visual style; default pixar." },
        vertical: { type: "boolean", description: "9:16 for phones instead of 16:9." },
        brief: { type: "string", description: "Optional direction: audience, message, tone." },
      },
      required: ["url"],
    },
    annotations: { title: "Make a promo video", readOnlyHint: false, destructiveHint: false, openWorldHint: true },
  },
  {
    name: "promo_status",
    description: "Progress of a promo_video job (brand, script, frames, motion, voice, music, render), and the MP4 and SRT links when it is done (fetch them with the same Bearer key).",
    inputSchema: { type: "object", properties: { id: { type: "string" } }, required: ["id"] },
    annotations: { title: "Promo video status", readOnlyHint: true, openWorldHint: false },
  },
  {
    name: "whoami",
    description: "The signed-in c0mpute account (CoinPay DID).",
    inputSchema: { type: "object", properties: {} },
    annotations: { title: "Who am I", readOnlyHint: true, openWorldHint: false },
  },
] as const;

type Rpc = { jsonrpc?: string; id?: string | number | null; method?: string; params?: Record<string, unknown> };
export type McpCtx = {
  user: { id: string; did: string; name: string | null };
  upstream: Upstream;
  push?: PushStore;
  /** Chat plugins: the store plus the add/run functions (plugins.ts), when wired. */
  plugins?: PluginMcpDeps & {
    hasEnabled: (userId: string) => Promise<boolean>;
    run: (o: { userId: string; upstream: Upstream; req: ChatRequest; emit: (e: PluginEvent) => void; signal?: AbortSignal }) => Promise<{ text: string }>;
  };
  /** The HTTP request's signal: the client hanging up stops the call. */
  signal?: AbortSignal;
  /** The account's synced conversations (convo-store.ts), when wired. */
  convos?: ConvoStore;
  /** Who started which generation job (media-owners.ts), when wired. */
  jobs?: { record: (userId: string, kind: "promo", jobId: unknown) => Promise<void>; owns: (userId: string, jobId: string) => Promise<boolean> };
};

export const CONVO_TOOLS = [
  {
    name: "chat_conversations",
    description: "This account's chat conversations (synced across the c0mpute.com chat, the TUI and the CLI), newest first: id, title, model, last change. Pass an id to chat's `conversation` to continue one.",
    inputSchema: { type: "object", properties: { limit: { type: "number", description: "At most this many (default 20, max 100)." } } },
    annotations: { title: "List conversations", readOnlyHint: true, openWorldHint: false },
  },
  {
    name: "chat_conversation",
    description: "One synced conversation's transcript (to recover context after a disconnect, or to read what was said on another device).",
    inputSchema: { type: "object", properties: { id: { type: "string" } }, required: ["id"] },
    annotations: { title: "Read a conversation", readOnlyHint: true, openWorldHint: false },
  },
];
const CONVO_TOOL_NAMES = new Set(CONVO_TOOLS.map((t) => t.name));

/**
 * tools/call requests still running, by user and JSON-RPC id, so a client's
 * `notifications/cancelled` (Esc in Claude Code, Stop in a chat app) can
 * interrupt a chat and its plugin tool loop instead of letting it run on.
 */
const inflight = new Map<string, AbortController>();
const inflightKey = (userId: string, id: unknown) => `${userId}\u0000${JSON.stringify(id)}`;

/** Tests: how many calls are running. */
export function inflightCalls(): number {
  return inflight.size;
}

const ok = (id: Rpc["id"], result: unknown) => ({ jsonrpc: "2.0", id: id ?? null, result });
const fail = (id: Rpc["id"], code: number, message: string) => ({ jsonrpc: "2.0", id: id ?? null, error: { code, message } });
const text = (t: string, isError = false) => ({ content: [{ type: "text", text: t }], ...(isError ? { isError } : {}) });

/** One JSON-RPC message in, one response out (null for notifications). */
export async function handleRpc(msg: Rpc, ctx: McpCtx): Promise<object | null> {
  if (!msg || typeof msg !== "object" || Array.isArray(msg)) return fail(null, -32600, "invalid request");
  const { id, method, params = {} } = msg;
  const isNotification = id === undefined;
  switch (method) {
    case "initialize":
      return ok(id, {
        protocolVersion: PROTOCOL_VERSION,
        capabilities: { tools: { listChanged: false } },
        serverInfo: { name: "c0mpute", title: "c0mpute", version: "1.1.0" },
        instructions:
          "c0mpute is a peer-to-peer network of everyday computers. hosting_push publishes a website (inline HTML/CSS/JS files) on the network with at least three self-healing copies, free through the shared cloud node when the person has no node of their own; poll hosting_push_status until it is live and give them the url (https://gateway.c0mpute.com/s/<site_id>/). hosting_pushes lists their sites, hosting_push_rm takes one down. chat asks open models on the network (list_models first); with chat plugins on (chat_plugins_list / chat_plugins_add: any remote MCP server, sign-in by link) those models can use the plugins' tools. promo_video makes a promo video of a website; poll promo_status.",
      });
    case "ping":
      return ok(id, {});
    case "notifications/cancelled": {
      const requestId = (params as { requestId?: unknown }).requestId;
      if (requestId !== undefined) inflight.get(inflightKey(ctx.user.id, requestId))?.abort();
      return null;
    }
    case "tools/list":
      return ok(id, { tools: [...TOOLS, ...(ctx.convos ? CONVO_TOOLS : []), ...(ctx.push ? HOSTING_TOOLS : []), ...(ctx.plugins ? PLUGIN_TOOLS : [])] });
    case "tools/call": {
      const name = params.name as string;
      const args = (params.arguments ?? {}) as Record<string, unknown>;
      const stop = new AbortController();
      const key = isNotification ? null : inflightKey(ctx.user.id, id);
      if (key) inflight.set(key, stop);
      const signal = ctx.signal ? AbortSignal.any([ctx.signal, stop.signal]) : stop.signal;
      try {
        if (CONVO_TOOL_NAMES.has(name)) {
          if (!ctx.convos) return fail(id, -32602, `unknown tool: ${name}`);
          if (name === "chat_conversations") {
            const limit = Math.min(100, Math.max(1, Number(args.limit) || 20));
            const list = (await ctx.convos.list(ctx.user.id)).slice(0, limit);
            if (!list.length) return ok(id, text("no conversations on this account yet"));
            return ok(id, text(list.map((c) => `${c.id}  ${new Date(c.updated).toISOString().slice(0, 16)}Z  ${c.model}  ${c.title}`).join("\n")));
          }
          const cid = typeof args.id === "string" ? args.id : "";
          const c = CONVO_ID.test(cid) ? await ctx.convos.get(ctx.user.id, cid) : null;
          if (!c) return ok(id, text(`no conversation ${cid} on this account`, true));
          const lines = c.messages.map((m) => `${m.role === "user" ? "user" : m.role === "assistant" ? "assistant" : m.role}: ${m.content}`);
          return ok(id, text(`# ${c.title} (${c.id}, model ${c.model})\n\n${lines.join("\n\n")}`));
        }
        if (HOSTING_TOOL_NAMES.has(name)) {
          if (!ctx.push) return fail(id, -32602, `unknown tool: ${name}`);
          const r = await hostingTool(name, args, ctx.user.id, ctx.push);
          return ok(id, text(r.text, r.isError));
        }
        if (PLUGIN_TOOL_NAMES.has(name)) {
          if (!ctx.plugins) return fail(id, -32602, `unknown tool: ${name}`);
          const r = await pluginTool(name, args, ctx.user.id, ctx.plugins);
          return ok(id, text(r.text, r.isError));
        }
        if (name === "whoami") return ok(id, text(JSON.stringify({ did: ctx.user.did, name: ctx.user.name })));
        if (name === "list_models") return ok(id, text((await listModels(ctx.upstream)).join("\n") || "(no models online)"));
        if (name === "promo_video") {
          const req = parsePublicPromoRequest(args);
          if (typeof req === "string") return ok(id, text(req, true));
          const res = await ctx.upstream("/api/chat/promo", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(req),
            signal: AbortSignal.timeout(20_000),
          });
          const body = (await res.json().catch(() => ({}))) as Record<string, unknown>;
          if (res.status !== 202 || typeof body.id !== "string") return ok(id, text(typeof body.error === "string" ? body.error : `promo could not start (HTTP ${res.status})`, true));
          await ctx.jobs?.record(ctx.user.id, "promo", body.id);
          return ok(id, text(JSON.stringify({ id: body.id, status: "pending", poll: `${site()}/api/v1/promo/${body.id}`, next: "call promo_status with this id every 30-60 s" })));
        }
        if (name === "promo_status") {
          const jobId = typeof args.id === "string" ? args.id : "";
          if (!UUID.test(jobId)) return ok(id, text("promo_status needs the id promo_video returned", true));
          if (ctx.jobs && !(await ctx.jobs.owns(ctx.user.id, jobId))) return ok(id, text("unknown promo job", true));
          const res = await ctx.upstream(`/api/chat/promo/${jobId}`, { signal: AbortSignal.timeout(20_000) });
          const body = (await res.json().catch(() => ({}))) as Record<string, unknown>;
          if (res.status === 404) return ok(id, text(typeof body.error === "string" ? body.error : "unknown promo job", true));
          const p = sanitizePromoProgress(jobId, body);
          if (p.status === "failed") return ok(id, text(`promo failed: ${p.error}`, true));
          const base = `${site()}/api/v1/promo/${jobId}`;
          return ok(id, text(JSON.stringify({
            id: jobId,
            status: p.status,
            seconds: p.seconds,
            progress: describePromoProgress(p),
            ...(p.status === "done" ? { video: `${base}/video`, captions: `${base}/captions`, duration: p.duration, motion: p.motion, title: p.title } : {}),
          })));
        }
        if (name === "chat") {
          let messages: ChatMessage[] = [];
          if (Array.isArray(args.messages) && args.messages.length) messages = args.messages as ChatMessage[];
          else if (typeof args.prompt === "string" && args.prompt) messages = [{ role: "user", content: args.prompt }];
          else return ok(id, text("chat needs `prompt` or `messages`", true));
          // A synced conversation: its history goes first, and the turn is saved to it.
          const convoId = typeof args.conversation === "string" && args.conversation ? args.conversation : null;
          if (convoId && !ctx.convos) return ok(id, text("conversations are not available on this server", true));
          if (convoId && !CONVO_ID.test(convoId)) return ok(id, text("conversation ids are 4-64 of A-Z a-z 0-9 _ -", true));
          const stored = convoId && ctx.convos ? await ctx.convos.get(ctx.user.id, convoId) : null;
          const fresh = messages.slice();
          if (stored) {
            const prior = stored.messages
              .filter((m) => (m.role === "user" || m.role === "assistant" || m.role === "system") && !m.error && m.content)
              .map((m) => ({ role: m.role as ChatMessage["role"], content: m.content }));
            messages = [...prior, ...messages];
          }
          if (typeof args.system === "string" && args.system) messages = [{ role: "system", content: args.system }, ...messages];
          const req = parseChatRequest({ ...args, messages, stream: false });
          if (typeof req === "string") return ok(id, text(req, true));
          let answer: string;
          let shown: string;
          if (ctx.plugins && args.plugins !== false && (await ctx.plugins.hasEnabled(ctx.user.id))) {
            const events: PluginEvent[] = [];
            const r = await ctx.plugins.run({ userId: ctx.user.id, upstream: ctx.upstream, req, emit: (e) => events.push(e), signal });
            answer = r.text;
            shown = r.text + describeSteps(events);
          } else {
            const r = await chatText(ctx.upstream, req, signal);
            answer = r.text;
            shown = r.text;
          }
          if (convoId && ctx.convos) {
            const now = Date.now();
            const firstUser = fresh.find((m) => m.role === "user")?.content ?? "";
            await ctx.convos
              .put(ctx.user.id, {
                id: convoId,
                title: stored?.title ?? (firstUser.slice(0, 60) || "new chat"),
                model: typeof args.model === "string" && args.model ? args.model : (stored?.model ?? "auto"),
                messages: [...(stored?.messages ?? []), ...fresh.filter((m) => m.role !== "system"), { role: "assistant", content: answer }],
                updated: Math.max(now, (stored?.updated ?? 0) + 1),
              })
              .catch(() => {});
          }
          return ok(id, text(shown));
        }
        return fail(id, -32602, `unknown tool: ${name}`);
      } catch (e) {
        if (stop.signal.aborted) return ok(id, text("cancelled", true));
        return ok(id, text(e instanceof Error ? e.message : String(e), true));
      } finally {
        if (key && inflight.get(key) === stop) inflight.delete(key);
      }
    }
    default:
      if (isNotification) return null;
      return fail(id, -32601, `method not found: ${method}`);
  }
}
