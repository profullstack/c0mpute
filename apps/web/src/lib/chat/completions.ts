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

/**
 * The answer text, for MCP and server callers. Streamed underneath
 * (streamChatText): no total ceiling, so a slow node that keeps writing
 * finishes; the caller's signal still ends it at once.
 */
export async function chatText(upstream: Upstream, req: ChatRequest, signal?: AbortSignal, onStatus?: (message: string) => void): Promise<{ text: string; model: string }> {
  const r = await streamChatText(upstream, req, { signal, onStatus });
  return { text: r.text, model: r.model };
}

/** An upstream failure that keeps its HTTP status (402 top up, 429, 503...). */
export class UpstreamError extends Error {
  constructor(readonly status: number, message: string) {
    super(message);
  }
}

/**
 * How long a streamed step may wait for its first token, all told: headers,
 * the network's queue and its retries. Status lines and keepalives do not
 * reset it (2026-11-28: they did, and a job still queued for a free slot
 * was failed as "the node stopped answering" 120 s after its last status).
 */
export const STEP_FIRST_BYTE_MS = 600_000;
/** How long a streamed step may go without a byte once the node is writing tokens. */
export const STEP_IDLE_MS = 120_000;

export type StreamedText = { text: string; model: string; finishReason?: string; usage?: { prompt_tokens?: number; completion_tokens?: number } };

/**
 * The answer text like chatText, but streamed from the node so the caller
 * hears progress (the network's own `: status` lines and a running token
 * count) and a slow node that keeps writing is never cut off: the only
 * timeouts are a first-byte wait and an idle gap, not a total ceiling.
 * chatText's 5-minute ceiling killed long plugin-chat answers from 9B nodes
 * at ~2 tok/s with "The operation timed out." after minutes of silence
 * (2026-11-28). A JSON (non-SSE) answer is accepted too.
 */
export async function streamChatText(
  upstream: Upstream,
  req: ChatRequest,
  opts: { signal?: AbortSignal; onStatus?: (message: string) => void; firstByteMs?: number; idleMs?: number } = {},
): Promise<StreamedText> {
  const idle = new AbortController();
  const signal = opts.signal ? AbortSignal.any([opts.signal, idle.signal]) : idle.signal;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let started = false;
  const arm = () => {
    clearTimeout(timer);
    const ms = started ? (opts.idleMs ?? STEP_IDLE_MS) : (opts.firstByteMs ?? STEP_FIRST_BYTE_MS);
    const why = started
      ? `the node stopped answering (nothing for ${Math.round(ms / 1000)} s); try again`
      : `no node started on this within ${Math.round(ms / 60_000)} min; the network is busy, try again in a minute`;
    timer = setTimeout(() => idle.abort(new DOMException(why, "TimeoutError")), ms);
  };
  const reason = (e: unknown) => (idle.signal.aborted && idle.signal.reason instanceof Error ? idle.signal.reason : e);
  arm();
  try {
    const res = await forwardChat(upstream, { ...req, stream: true }, signal);
    const type = res.headers.get("content-type") ?? "";
    if (!type.includes("event-stream") || !res.body) {
      if (!type.includes("json")) {
        // An nginx/HTML page (Infernet restarting, a deploy).
        await res.body?.cancel().catch(() => {});
        if (!res.ok) throw new UpstreamError(503, `the c0mpute network is restarting or busy (upstream HTTP ${res.status}); try again in a minute`);
      }
      const body = (await res.json().catch(() => ({}))) as {
        model?: string;
        choices?: { message?: { content?: string }; finish_reason?: string }[];
        usage?: { prompt_tokens?: number; completion_tokens?: number };
        error?: { message?: string };
      };
      if (!res.ok || body.error) throw new UpstreamError(res.ok ? 502 : res.status, body.error?.message || `inference failed (HTTP ${res.status})`);
      return {
        text: body.choices?.[0]?.message?.content ?? "",
        model: body.model ?? req.model ?? "auto",
        finishReason: body.choices?.[0]?.finish_reason,
        usage: body.usage,
      };
    }
    const reader = res.body.getReader();
    const dec = new TextDecoder();
    let buf = "";
    let text = "";
    let model = req.model ?? "auto";
    let pieces = 0;
    let lastNote = 0;
    let finishReason: string | undefined;
    let usage: StreamedText["usage"];
    const result = (): StreamedText => ({ text, model, finishReason, usage });
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (!value?.byteLength) continue;
      // Before the first token only the fixed queue budget runs; after it,
      // any byte (token, status, keepalive) proves the node is alive.
      if (started) arm();
      buf += dec.decode(value, { stream: true });
      let nl;
      while ((nl = buf.indexOf("\n")) !== -1) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (line.startsWith(": status")) {
          try {
            const s = JSON.parse(line.slice(8).trim()) as { message?: unknown };
            if (typeof s.message === "string" && s.message) opts.onStatus?.(s.message);
          } catch {}
          continue;
        }
        if (!line.startsWith("data:")) continue;
        const data = line.slice(5).trim();
        if (data === "[DONE]") {
          void reader.cancel().catch(() => {});
          return result();
        }
        let j: {
          model?: string;
          error?: { message?: string };
          choices?: { delta?: { content?: string }; finish_reason?: string | null }[];
          usage?: { prompt_tokens?: number; completion_tokens?: number };
        };
        try {
          j = JSON.parse(data);
        } catch {
          continue;
        }
        if (j.error) throw new UpstreamError(502, j.error.message || "inference failed");
        if (typeof j.model === "string" && j.model) model = j.model;
        if (j.choices?.[0]?.finish_reason) finishReason = j.choices[0].finish_reason;
        if (j.usage) usage = j.usage;
        const piece = j.choices?.[0]?.delta?.content;
        if (typeof piece === "string" && piece) {
          if (!started) {
            started = true;
            arm();
          }
          text += piece;
          pieces++;
          const t = Date.now();
          if (t - lastNote >= 1000) {
            lastNote = t;
            opts.onStatus?.(`a node is writing… ${pieces} tokens`);
          }
        }
      }
    }
    return result();
  } catch (e) {
    throw reason(e);
  } finally {
    clearTimeout(timer);
  }
}

export async function listModels(upstream: Upstream): Promise<string[]> {
  const res = await upstream("/v1/models", { signal: AbortSignal.timeout(15_000) });
  if (!res.ok) throw new Error(`models HTTP ${res.status}`);
  const body = (await res.json()) as { data?: { id: string }[] };
  return (body.data ?? []).map((m) => m.id);
}
