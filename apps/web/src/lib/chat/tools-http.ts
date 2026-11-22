// Serving a /v1/chat/completions request that carries `tools` (lib/chat/tools.ts
// has the format). One ordinary chat job per model call through Infernet, so
// the auction, the person's balance and the 5% house fee see a normal job; the
// only extra call is the single repair retry when a reply looked like a tool
// call that could not be read.

import { parseAutoModel } from "@/lib/router";
import { C0_SYSTEM_PROMPT, forwardChat, listModels, type ChatMessage, type ChatRequest, type Upstream } from "./completions";
import {
  applyChoice,
  autoToolModels,
  estimateUsage,
  needsRepair,
  parseToolReply,
  repairMessage,
  supportsTools,
  toolCompletionJson,
  toolStreamChunks,
  withToolsPrompt,
  type ParsedReply,
  type Tooling,
} from "./tools";

export const TOOLS_HEADER = "x-c0mpute-tools";

/** Thrown for errors the client should see with their own status (400, 402, 503...). */
export class ToolChatError extends Error {
  constructor(readonly status: number, message: string) {
    super(message);
  }
}

type CallResult = { text: string; model: string };

/** One non-streamed model call; throws ToolChatError carrying the upstream status. */
export async function callModel(upstream: Upstream, model: string, messages: ChatMessage[], req: ChatRequest, signal?: AbortSignal): Promise<CallResult> {
  const timeout = AbortSignal.timeout(240_000);
  const res = await forwardChat(
    upstream,
    { model, messages, stream: false, ...(req.max_tokens ? { max_tokens: req.max_tokens } : {}), ...(req.temperature !== undefined ? { temperature: req.temperature } : {}) },
    signal ? AbortSignal.any([signal, timeout]) : timeout,
  );
  const type = res.headers.get("content-type") ?? "";
  if (!type.includes("json")) {
    await res.body?.cancel().catch(() => {});
    throw new ToolChatError(503, `the c0mpute network is restarting or busy (upstream HTTP ${res.status}); try again in a minute`);
  }
  const body = (await res.json().catch(() => ({}))) as { model?: string; choices?: { message?: { content?: string } }[]; error?: { message?: string } };
  if (!res.ok) throw new ToolChatError(res.status, body.error?.message || `inference failed (HTTP ${res.status})`);
  return { text: body.choices?.[0]?.message?.content ?? "", model: body.model ?? model };
}

let modelsCache: { at: number; ids: string[] } | null = null;
/** Tests: forget the cached model list. */
export function resetModelsCache() {
  modelsCache = null;
}
async function liveModels(upstream: Upstream): Promise<string[]> {
  if (modelsCache && Date.now() - modelsCache.at < 60_000) return modelsCache.ids;
  const ids = await listModels(upstream);
  modelsCache = { at: Date.now(), ids };
  return ids;
}

/** The models to try, best first; a 400/503 ToolChatError when there are none. */
export async function toolModels(upstream: Upstream, req: ChatRequest, rawModel: unknown): Promise<string[]> {
  if (req.model) {
    if (!supportsTools(req.model)) {
      let live: string[] = [];
      try {
        live = (await liveModels(upstream)).filter(supportsTools);
      } catch {
        /* the list is only a hint */
      }
      throw new ToolChatError(
        400,
        `model "${req.model}" does not support tool calling on c0mpute${live.length ? `; tool-capable models online now: ${live.join(", ")} (or "auto")` : '; use "auto"'}`,
      );
    }
    return [req.model];
  }
  const effort = parseAutoModel(rawModel) ?? "standard";
  const picked = autoToolModels(await liveModels(upstream), effort).slice(0, 3);
  if (!picked.length) throw new ToolChatError(503, "no tool-capable model is online on the c0mpute network right now; try again shortly");
  return picked;
}

export type ToolRun = { reply: ParsedReply; model: string; raw: string; prompt: ChatMessage[]; attempts: number };

/**
 * Run the request: each candidate in turn until one answers (a busy or
 * failing node hands over to the next; a 4xx such as 402 or 429 stops at
 * once), then parse the reply, with one repair retry on the same model.
 */
export async function runTools(upstream: Upstream, req: ChatRequest, tooling: Tooling, candidates: string[], signal?: AbortSignal, budgetMs = 230_000): Promise<ToolRun> {
  const prompt = withToolsPrompt(req.messages, tooling, C0_SYSTEM_PROMPT);
  const t0 = Date.now();
  let attempts = 0;
  let last: ToolChatError | null = null;
  for (const model of candidates) {
    if (attempts > 0 && Date.now() - t0 > budgetMs) break;
    attempts++;
    let first: CallResult;
    try {
      first = await callModel(upstream, model, prompt, req, signal);
    } catch (e) {
      if (signal?.aborted) throw e;
      const err = e instanceof ToolChatError ? e : new ToolChatError(502, e instanceof Error ? e.message : String(e));
      if (err.status < 500) throw err;
      last = err;
      continue;
    }
    let reply = applyChoice(parseToolReply(first.text, tooling.tools), tooling);
    let raw = first.text;
    if (needsRepair(reply, tooling) && Date.now() - t0 < budgetMs) {
      attempts++;
      try {
        const again = await callModel(upstream, model, [...prompt, { role: "assistant", content: first.text }, { role: "user", content: repairMessage(tooling) }], req, signal);
        const fixed = applyChoice(parseToolReply(again.text, tooling.tools), tooling);
        if (fixed.calls.length) {
          reply = fixed;
          raw = again.text;
        }
      } catch (e) {
        if (signal?.aborted) throw e;
        /* keep the first reply */
      }
    }
    // An unreadable tool call never reaches the client as fake prose.
    if (reply.attempted && !reply.calls.length) {
      last = new ToolChatError(502, `model "${model}" returned a tool call that could not be parsed`);
      continue;
    }
    return { reply, model: first.model, raw, prompt, attempts };
  }
  throw last ?? new ToolChatError(503, "no tool-capable model could take this request; try again shortly");
}

const enc = new TextEncoder();
const sse = (obj: unknown) => enc.encode(`data: ${JSON.stringify(obj)}\n\n`);
const errorBody = (status: number, message: string) => ({ error: { message, type: "c0mpute_error", code: status } });

/** The HTTP response for a tool request: JSON, or OpenAI SSE with tool_calls deltas. */
export async function toolChatResponse(a: { upstream: Upstream; req: ChatRequest; rawModel: unknown; signal?: AbortSignal }): Promise<Response> {
  const tooling = a.req.tooling as Tooling;
  const headers = { "cache-control": "no-store", "x-accel-buffering": "no" };
  let candidates: string[];
  try {
    candidates = await toolModels(a.upstream, a.req, a.rawModel);
  } catch (e) {
    const status = e instanceof ToolChatError ? e.status : 503;
    return new Response(JSON.stringify(errorBody(status, e instanceof Error ? e.message : String(e))), {
      status,
      headers: { ...headers, "content-type": "application/json" },
    });
  }
  const work = runTools(a.upstream, a.req, tooling, candidates, a.signal);
  const id = `chatcmpl-c0t-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;

  if (a.req.stream) {
    const includeUsage = a.req.include_usage === true;
    const body = new ReadableStream<Uint8Array>({
      async start(controller) {
        // Nothing can be shown before the reply is parsed: keep the connection warm.
        controller.enqueue(enc.encode(": c0mpute tools\n\n"));
        const ping = setInterval(() => {
          try {
            controller.enqueue(enc.encode(": c0mpute tools\n\n"));
          } catch {
            /* closed */
          }
        }, 10_000);
        try {
          const r = await work;
          for (const c of toolStreamChunks(r.reply, r.model, includeUsage ? estimateUsage(r.prompt, r.raw) : null, id)) controller.enqueue(sse(c));
        } catch (e) {
          const status = e instanceof ToolChatError ? e.status : 502;
          controller.enqueue(sse(errorBody(status, e instanceof Error ? e.message : String(e))));
        } finally {
          clearInterval(ping);
        }
        controller.enqueue(enc.encode("data: [DONE]\n\n"));
        controller.close();
      },
    });
    return new Response(body, { headers: { ...headers, "content-type": "text/event-stream", [TOOLS_HEADER]: `format=hermes; candidates=${candidates.join(",")}` } });
  }

  try {
    const r = await work;
    return new Response(JSON.stringify(toolCompletionJson(r.reply, r.model, estimateUsage(r.prompt, r.raw), id)), {
      headers: { ...headers, "content-type": "application/json", [TOOLS_HEADER]: `format=hermes; model=${r.model}; attempts=${r.attempts}` },
    });
  } catch (e) {
    const status = e instanceof ToolChatError ? e.status : 502;
    return new Response(JSON.stringify(errorBody(status, e instanceof Error ? e.message : String(e))), {
      status,
      headers: { ...headers, "content-type": "application/json", ...(status === 503 ? { "retry-after": "30" } : {}) },
    });
  }
}
