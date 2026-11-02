// MCP (JSON-RPC 2.0, Streamable HTTP with plain JSON responses) for c0mpute chat.
// Tools: chat, list_models, whoami. Auth is the same c0k_ API key as the REST API.
import { chatText, listModels, parseChatRequest, type ChatMessage, type Upstream } from "./completions";

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
  },
  {
    name: "list_models",
    description: "Models live on the c0mpute network right now.",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "whoami",
    description: "The signed-in c0mpute account (CoinPay DID).",
    inputSchema: { type: "object", properties: {} },
  },
] as const;

type Rpc = { jsonrpc?: string; id?: string | number | null; method?: string; params?: Record<string, unknown> };
export type McpCtx = { user: { id: string; did: string; name: string | null }; upstream: Upstream };

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
        serverInfo: { name: "c0mpute-chat", version: "1.0.0" },
        instructions: "Chat with models on the c0mpute network. Call list_models, then chat.",
      });
    case "ping":
      return ok(id, {});
    case "tools/list":
      return ok(id, { tools: TOOLS });
    case "tools/call": {
      const name = params.name as string;
      const args = (params.arguments ?? {}) as Record<string, unknown>;
      try {
        if (name === "whoami") return ok(id, text(JSON.stringify({ did: ctx.user.did, name: ctx.user.name })));
        if (name === "list_models") return ok(id, text((await listModels(ctx.upstream)).join("\n") || "(no models online)"));
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
