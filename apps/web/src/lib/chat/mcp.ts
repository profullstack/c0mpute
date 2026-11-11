// MCP (JSON-RPC 2.0, Streamable HTTP with plain JSON responses) for c0mpute chat.
// Tools: chat, list_models, whoami, promo_video, promo_status. Auth is the same
// c0k_ API key as the REST API.
import { chatText, listModels, parseChatRequest, type ChatMessage, type Upstream } from "./completions";
import { describePromoProgress, parsePromoRequest, sanitizePromoProgress, PROMO_STYLES } from "./promo";
import { HOSTING_TOOL_NAMES, HOSTING_TOOLS, hostingTool } from "./push-mcp";
import type { PushStore } from "./push";

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
export type McpCtx = { user: { id: string; did: string; name: string | null }; upstream: Upstream; push?: PushStore };

const ok = (id: Rpc["id"], result: unknown) => ({ jsonrpc: "2.0", id: id ?? null, result });
const fail = (id: Rpc["id"], code: number, message: string) => ({ jsonrpc: "2.0", id: id ?? null, error: { code, message } });
const text = (t: string, isError = false) => ({ content: [{ type: "text", text: t }], ...(isError ? { isError } : {}) });

/** One JSON-RPC message in, one response out (null for notifications). */
export async function handleRpc(msg: Rpc, ctx: McpCtx): Promise<object | null> {
  const { id, method, params = {} } = msg;
  const isNotification = id === undefined;
  switch (method) {
    case "initialize":
      return ok(id, {
        protocolVersion: PROTOCOL_VERSION,
        capabilities: { tools: { listChanged: false } },
        serverInfo: { name: "c0mpute", title: "c0mpute", version: "1.1.0" },
        instructions:
          "c0mpute is a peer-to-peer network of everyday computers. hosting_push publishes a website (inline HTML/CSS/JS files) on the network with at least three self-healing copies, free through the shared cloud node when the person has no node of their own; poll hosting_push_status until it is live and give them the url (https://gateway.c0mpute.com/s/<site_id>/). hosting_pushes lists their sites, hosting_push_rm takes one down. chat asks open models on the network (list_models first). promo_video makes a promo video of a website; poll promo_status.",
      });
    case "ping":
      return ok(id, {});
    case "tools/list":
      return ok(id, { tools: ctx.push ? [...TOOLS, ...HOSTING_TOOLS] : TOOLS });
    case "tools/call": {
      const name = params.name as string;
      const args = (params.arguments ?? {}) as Record<string, unknown>;
      try {
        if (HOSTING_TOOL_NAMES.has(name)) {
          if (!ctx.push) return fail(id, -32602, `unknown tool: ${name}`);
          const r = await hostingTool(name, args, ctx.user.id, ctx.push);
          return ok(id, text(r.text, r.isError));
        }
        if (name === "whoami") return ok(id, text(JSON.stringify({ did: ctx.user.did, name: ctx.user.name })));
        if (name === "list_models") return ok(id, text((await listModels(ctx.upstream)).join("\n") || "(no models online)"));
        if (name === "promo_video") {
          const req = parsePromoRequest(args);
          if (typeof req === "string") return ok(id, text(req, true));
          const res = await ctx.upstream("/api/chat/promo", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(req),
            signal: AbortSignal.timeout(20_000),
          });
          const body = (await res.json().catch(() => ({}))) as Record<string, unknown>;
          if (res.status !== 202 || typeof body.id !== "string") return ok(id, text(typeof body.error === "string" ? body.error : `promo could not start (HTTP ${res.status})`, true));
          return ok(id, text(JSON.stringify({ id: body.id, status: "pending", poll: `${site()}/api/v1/promo/${body.id}`, next: "call promo_status with this id every 30-60 s" })));
        }
        if (name === "promo_status") {
          const jobId = typeof args.id === "string" ? args.id : "";
          if (!UUID.test(jobId)) return ok(id, text("promo_status needs the id promo_video returned", true));
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
          if (typeof args.system === "string" && args.system) messages = [{ role: "system", content: args.system }, ...messages];
          const req = parseChatRequest({ ...args, messages, stream: false });
          if (typeof req === "string") return ok(id, text(req, true));
          const r = await chatText(ctx.upstream, req);
          return ok(id, text(r.text));
        }
        return fail(id, -32602, `unknown tool: ${name}`);
      } catch (e) {
        return ok(id, text(e instanceof Error ? e.message : String(e), true));
      }
    }
    default:
      if (isNotification) return null;
      return fail(id, -32601, `method not found: ${method}`);
  }
}
