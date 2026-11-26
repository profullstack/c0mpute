// OpenAI-shaped chat on the c0mpute network. Every request runs on an Infernet
// provider node (p2p_only: never the paid hosted fallback) under c0mpute's
// trusted client key; c0mpute users are rate-limited per account here.

import { parseAutoModel } from "@/lib/router";
import { parseTooling, renderMessages, type Tooling } from "./tools";
import { expandCommands, usedCommand } from "./command-registry";
import { contextTokensFromEnv, fitContext, promptBudget } from "./context-budget";
import { captureWorker, conversationKey, preferredWorker } from "./affinity";

export type ChatMessage = { role: "system" | "user" | "assistant"; content: string };
export type ChatRequest = {
  model?: string;
  messages: ChatMessage[];
  stream?: boolean;
  temperature?: number;
  max_tokens?: number;
  /** OpenAI tools / tool_choice / parallel_tool_calls (lib/chat/tools.ts). Never forwarded as-is. */
  tooling?: Tooling;
  /** stream_options.include_usage */
  include_usage?: boolean;
  /** The prompt command (`/analyze`, commands/*.md) this conversation used last. Never forwarded. */
  command?: string;
};

const MAX_CHARS = 200_000;
const MAX_MESSAGES = 200;

/**
 * Validate a client body and strip it down to what we forward. Prompt
 * commands in user turns are expanded (lib/chat/command-registry.ts). Messages come
 * back as plain system/user/assistant strings: content-part arrays are
 * flattened to their text, and tool history (assistant tool_calls, role
 * "tool") is rendered into the `<tool_call>` / `<tool_response>` format the
 * models were trained on (lib/chat/tools.ts).
 */
export function parseChatRequest(body: unknown): ChatRequest | string {
  if (!body || typeof body !== "object") return "body must be a JSON object";
  const b = body as Record<string, unknown>;
  if (!Array.isArray(b.messages) || b.messages.length === 0) return "messages[] is required";
  if (b.messages.length > MAX_MESSAGES) return `at most ${MAX_MESSAGES} messages`;
  const rendered = renderMessages(b.messages);
  if (typeof rendered === "string") return rendered;
  // Prompt commands (`/analyze <question>`, commands/*.md): the command's
  // instructions join the system turn and the user turn keeps the question.
  const messages = expandCommands(rendered, C0_SYSTEM_PROMPT);
  const chars = messages.reduce((n, m) => n + m.content.length, 0);
  if (chars > MAX_CHARS) return `messages total more than ${MAX_CHARS} characters`;
  const tooling = parseTooling(b);
  if (typeof tooling === "string") return tooling;
  const jsonNote = responseFormatNote(b.response_format);
  if (jsonNote && typeof jsonNote === "object") return jsonNote.error;
  if (jsonNote) {
    // Into the caller's system prompt, or after the house prompt it would have had.
    const i = messages.findIndex((m) => m.role === "system");
    if (i >= 0) messages[i] = { role: "system", content: `${messages[i].content}\n\n${jsonNote}` };
    else messages.unshift({ role: "system", content: `${C0_SYSTEM_PROMPT}\n\n${jsonNote}` });
  }
  const out: ChatRequest = { messages, stream: b.stream === true };
  const cmd = usedCommand(rendered);
  if (cmd) out.command = cmd.name;
  // Prompt commands do arithmetic: sample tighter unless the caller chose a temperature.
  if (cmd && typeof b.temperature !== "number") out.temperature = 0.2;
  if (tooling && tooling.choice !== "none") out.tooling = tooling;
  const so = b.stream_options as Record<string, unknown> | undefined;
  if (so && typeof so === "object" && so.include_usage === true) out.include_usage = true;
  // "auto" and the router aliases ("auto:high", ...) are not model ids.
  if (typeof b.model === "string" && b.model && !parseAutoModel(b.model)) out.model = b.model.slice(0, 200);
  if (typeof b.temperature === "number" && Number.isFinite(b.temperature)) {
    out.temperature = Math.min(2, Math.max(0, b.temperature));
  }
  const maxTokens = typeof b.max_completion_tokens === "number" ? b.max_completion_tokens : b.max_tokens;
  if (typeof maxTokens === "number" && Number.isFinite(maxTokens)) {
    out.max_tokens = Math.min(8192, Math.max(1, Math.floor(maxTokens)));
  }
  return out;
}

/**
 * `response_format`: Infernet carries no format field to the node, so JSON
 * mode is an instruction appended as a system turn (the router also checks
 * the reply parses when it routes). "text" or absent: nothing.
 */
export function responseFormatNote(rf: unknown): string | null | { error: string } {
  if (rf === undefined || rf === null) return null;
  if (typeof rf !== "object") return { error: "response_format must be an object" };
  const r = rf as Record<string, unknown>;
  if (r.type === "text") return null;
  const base = "Respond with one valid JSON object only: no prose, no code fences.";
  if (r.type === "json_object") return base;
  if (r.type === "json_schema") {
    const js = r.json_schema as Record<string, unknown> | undefined;
    const schema = js && typeof js === "object" ? js.schema : undefined;
    return schema ? `${base} It must match this JSON Schema:\n${JSON.stringify(schema).slice(0, 8000)}` : base;
  }
  return { error: 'response_format.type must be "text", "json_object" or "json_schema"' };
}

/** The request as Infernet takes it: only the fields it reads. */
function wireBody(req: ChatRequest) {
  const { tooling: _t, include_usage: _u, command: _c, ...rest } = req;
  return rest;
}

export type Upstream = (path: string, init?: RequestInit) => Promise<Response>;

/**
 * Who is answering. Without it an open-weight model introduces itself from
 * its training data ("I'm Qwen, created by Alibaba Cloud, on servers in
 * Shanghai"), which is false here: the answer comes from a peer node.
 */
export const C0_SYSTEM_PROMPT =
  "You are c0, the assistant in c0mpute chat (c0mpute.com). Your replies are generated by an open-weight model " +
  "running on a peer node of the c0mpute / Infernet peer-to-peer compute network, not on any company's cloud. " +
  "If asked who you are, say you are c0 on the c0mpute network running an open-weight model; do not claim to be " +
  "a product of the model's original publisher or to run on their servers. You do not know which node or machine " +
  "is serving you. If asked about cost, say chats are billed to the user's c0mpute.com balance when they have one, " +
  "otherwise they use the free daily allowance, and the details are on c0mpute.com. Be concise and helpful.";

/**
 * Assistant turns that were never answers: the provider daemon's stub engine
 * and raw engine errors. Clients (the TUI, the PWA) keep them in history and
 * resend them, and a small model then copies the pattern word for word: after
 * a dozen stub turns qwen2.5:0.5b answered "we on?" with the stub text itself
 * (2026-11-11), long after the stub node was gone.
 */
const POISON_RE =
  /This is a stub response from the provider daemon|^ollama HTTP \d{3}:|^the c0mpute network is restarting or busy|^no (live node|Infernet node)/;

export function isPoisonedTurn(m: ChatMessage): boolean {
  return m.role === "assistant" && POISON_RE.test(m.content.trim());
}

/** Drop poisoned assistant turns, then prepend the c0 system prompt unless the caller sent its own. */
export function withHousePrompt(messages: ChatMessage[]): ChatMessage[] {
  const clean = messages.filter((m) => !isPoisonedTurn(m));
  if (clean.some((m) => m.role === "system")) return clean;
  return [{ role: "system", content: C0_SYSTEM_PROMPT }, ...clean];
}

/**
 * The messages a node receives: house prompt, then fitted to the node's
 * context window (lib/chat/context-budget.ts). Unchanged while they fit.
 */
export function wireMessages(req: ChatRequest, contextTokens = contextTokensFromEnv()): ChatMessage[] {
  const { messages, report } = fitContext(withHousePrompt(req.messages), promptBudget(contextTokens, req.max_tokens));
  if (report.compacted) {
    console.info(
      `chat context: compacted ${report.before} -> ${report.after} tokens (budget ${report.budget}, ` +
        `clipped ${report.clippedResults} tool results, dropped ${report.droppedMessages} messages)`,
    );
  }
  return messages;
}

/** Forward to Infernet; the Response (JSON or SSE) is handed back as-is. */
export async function forwardChat(upstream: Upstream, req: ChatRequest, signal?: AbortSignal) {
  // Affinity (lib/chat/affinity.ts): ask for the node that served this
  // conversation's last turn (its prompt cache is warm) and remember who
  // answers this one.
  const key = conversationKey(req.model, req.messages);
  const prefer = preferredWorker(key);
  const res = await upstream("/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json", accept: req.stream ? "text/event-stream" : "application/json" },
    body: JSON.stringify({ ...wireBody(req), messages: wireMessages(req), p2p_only: true, ...(prefer ? { prefer_worker: prefer } : {}) }),
    signal,
  });
  return res.ok ? captureWorker(res, key) : res;
}

/** Non-streaming convenience for MCP and server callers: the answer text. */
export async function chatText(upstream: Upstream, req: ChatRequest, signal?: AbortSignal): Promise<{ text: string; model: string }> {
  // The caller's signal (a closed connection, Stop, MCP cancel) ends the
  // upstream request at once, not at the 5-minute ceiling.
  const timeout = AbortSignal.timeout(300_000);
  const res = await forwardChat(upstream, { ...req, stream: false }, signal ? AbortSignal.any([timeout, signal]) : timeout);
  const body = (await res.json().catch(() => ({}))) as {
    model?: string;
    choices?: { message?: { content?: string } }[];
    error?: { message?: string };
  };
  if (!res.ok) throw new Error(body.error?.message || `inference failed (HTTP ${res.status})`);
  return { text: body.choices?.[0]?.message?.content ?? "", model: body.model ?? req.model ?? "auto" };
}

export async function listModels(upstream: Upstream): Promise<string[]> {
  const res = await upstream("/v1/models", { signal: AbortSignal.timeout(15_000) });
  if (!res.ok) throw new Error(`models HTTP ${res.status}`);
  const body = (await res.json()) as { data?: { id: string }[] };
  return (body.data ?? []).map((m) => m.id);
}
