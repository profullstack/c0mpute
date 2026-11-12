// The model router behind `model: "auto"` on /v1/chat/completions (DIP 0035).
//
// lib/router is framework-free; this file binds it to c0mpute: Infernet as
// the model call (billed to the signed-in person), the live model list and
// Infernet's price table as the pool, SQLite Cloud for what it learns, and
// OpenAI-shaped responses with the routing receipt attached.

import { Router, buildProfiles, classify, parseAutoModel, type CallFn, type Effort, type ModelProfile, type Msg, type PriceTable, type Receipt, type RouteOptions, type Snapshot, type StatsStore } from "@/lib/router";
import { forwardChat, isPoisonedTurn, listModels, type ChatRequest, type Upstream } from "./completions";
import type { Sql } from "./store";

export const ROUTE_HEADER = "x-c0mpute-route";

/** Routing on unless C0MPUTE_ROUTER=off. */
export function routerEnabled(env: (k: string) => string): boolean {
  return !/^(off|0|false|no)$/i.test(env("C0MPUTE_ROUTER").trim());
}

/**
 * Whether a request should be routed, and at what effort. `model` "auto",
 * "auto:<effort>" or absent routes; any named model goes straight through.
 * A body `router: { effort, max_cost_usd, max_latency_ms }` refines it.
 */
export function routeRequest(raw: Record<string, unknown> | null): RouteOptions | null {
  const model = raw?.model;
  const fromModel = model === undefined || model === null || model === "" ? "standard" : parseAutoModel(model);
  if (!fromModel) return null;
  const r = (raw?.router && typeof raw.router === "object" ? raw.router : {}) as Record<string, unknown>;
  const effort = (["fast", "standard", "high", "max"] as const).includes(r.effort as Effort) ? (r.effort as Effort) : fromModel;
  const num = (v: unknown, lo: number, hi: number) => (typeof v === "number" && Number.isFinite(v) ? Math.min(hi, Math.max(lo, v)) : undefined);
  return {
    effort,
    maxCostUsd: num(r.max_cost_usd, 0, 10),
    maxLatencyMs: num(r.max_latency_ms, 1000, 290_000),
  };
}

// ── persistence ──────────────────────────────────────────────────────────

export const ROUTER_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS router_stats (
     id         TEXT PRIMARY KEY,
     snapshot   TEXT NOT NULL,
     updated_at TEXT NOT NULL
   )`,
];

export function sqlStatsStore(sql: Sql, id = "global"): StatsStore {
  let ready: Promise<void> | null = null;
  const init = () => (ready ??= (async () => {
    for (const q of ROUTER_SCHEMA) await sql.run(q);
  })());
  return {
    async load() {
      await init();
      const rows = await sql.all(`SELECT snapshot FROM router_stats WHERE id = ?`, [id]);
      if (!rows[0]) return null;
      try {
        return JSON.parse(String(rows[0].snapshot)) as Snapshot;
      } catch {
        return null;
      }
    },
    async save(s: Snapshot) {
      await init();
      await sql.run(
        `INSERT INTO router_stats (id, snapshot, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET snapshot = excluded.snapshot, updated_at = excluded.updated_at`,
        [id, JSON.stringify(s), new Date().toISOString()],
      );
    },
  };
}

// ── the pool ─────────────────────────────────────────────────────────────

/** Live models + prices, refreshed at most once a minute. */
export function modelPool(upstream: Upstream, ttlMs = 60_000) {
  let cached: { at: number; models: ModelProfile[] } | null = null;
  let inflight: Promise<ModelProfile[]> | null = null;
  return async (): Promise<ModelProfile[]> => {
    if (cached && Date.now() - cached.at < ttlMs) return cached.models;
    inflight ??= (async () => {
      try {
        const [ids, prices] = await Promise.all([
          listModels(upstream),
          upstream("/api/v1/pricing", { signal: AbortSignal.timeout(10_000) })
            .then((r) => (r.ok ? (r.json() as Promise<PriceTable>) : undefined))
            .catch(() => undefined),
        ]);
        const models = buildProfiles(ids, prices);
        if (models.length) cached = { at: Date.now(), models };
        return models.length ? models : cached?.models ?? [];
      } catch (e) {
        if (cached) return cached.models;
        throw e;
      } finally {
        inflight = null;
      }
    })();
    return inflight;
  };
}

// ── model calls ──────────────────────────────────────────────────────────

/** One non-streamed model call through Infernet, billed via `upstream`. */
export function infernetCall(upstream: Upstream): CallFn {
  return async (model, messages, opts) => {
    const req: ChatRequest = { model, messages, stream: false };
    if (opts.maxTokens) req.max_tokens = opts.maxTokens;
    if (opts.temperature !== undefined) req.temperature = opts.temperature;
    const signal = opts.signal ? AbortSignal.any([opts.signal, AbortSignal.timeout(280_000)]) : AbortSignal.timeout(280_000);
    const res = await forwardChat(upstream, req, signal);
    const body = (await res.json().catch(() => ({}))) as {
      model?: string;
      choices?: { message?: { content?: string }; finish_reason?: string }[];
      usage?: { prompt_tokens?: number; completion_tokens?: number };
      error?: { message?: string };
    };
    if (!res.ok) throw new Error(body.error?.message || `HTTP ${res.status}`);
    return {
      text: body.choices?.[0]?.message?.content ?? "",
      model: body.model ?? model,
      finishReason: body.choices?.[0]?.finish_reason,
      inTokens: body.usage?.prompt_tokens,
      outTokens: body.usage?.completion_tokens,
    };
  };
}

// ── responses ────────────────────────────────────────────────────────────

/** Short header form of a receipt: pattern, final model, cost, time. */
export function routeHeader(r: Pick<Receipt, "plan" | "final" | "totalCostUsd" | "totalMs" | "escalated">): string {
  return `pattern=${r.plan.kind}; model=${r.final.model}; escalated=${r.escalated}; cost_usd=${r.totalCostUsd}; ms=${r.totalMs}`;
}

export function completionJson(text: string, model: string, receipt: Receipt) {
  const usage = receipt.steps.reduce(
    (u, s) => ({ prompt_tokens: u.prompt_tokens + s.inTokens, completion_tokens: u.completion_tokens + s.outTokens }),
    { prompt_tokens: 0, completion_tokens: 0 },
  );
  return {
    id: `chatcmpl-c0r-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`,
    object: "chat.completion",
    created: Math.floor(Date.now() / 1000),
    model,
    choices: [{ index: 0, message: { role: "assistant", content: text }, finish_reason: "stop" }],
    usage: { ...usage, total_tokens: usage.prompt_tokens + usage.completion_tokens },
    x_c0mpute_route: receipt,
  };
}

const enc = new TextEncoder();
const sse = (obj: unknown) => enc.encode(`data: ${JSON.stringify(obj)}\n\n`);

/**
 * SSE for a routed run: keepalive comments while the plan runs (cascades,
 * critique and fusion finish before anything can be shown), then the answer
 * in OpenAI chunk form, the receipt on the last chunk, then [DONE].
 */
export function routedStream(work: Promise<{ text: string; model: string; receipt: Receipt }>): ReadableStream<Uint8Array> {
  return new ReadableStream({
    async start(controller) {
      const ping = setInterval(() => controller.enqueue(enc.encode(": c0mpute routing\n\n")), 10_000);
      const id = `chatcmpl-c0r-${Date.now().toString(36)}`;
      const created = Math.floor(Date.now() / 1000);
      try {
        const { text, model, receipt } = await work;
        clearInterval(ping);
        controller.enqueue(sse({ id, object: "chat.completion.chunk", created, model, choices: [{ index: 0, delta: { role: "assistant" }, finish_reason: null }] }));
        for (let i = 0; i < text.length; i += 400) {
          controller.enqueue(sse({ id, object: "chat.completion.chunk", created, model, choices: [{ index: 0, delta: { content: text.slice(i, i + 400) }, finish_reason: null }] }));
        }
        controller.enqueue(sse({ id, object: "chat.completion.chunk", created, model, choices: [{ index: 0, delta: {}, finish_reason: "stop" }], x_c0mpute_route: receipt }));
      } catch (e) {
        clearInterval(ping);
        controller.enqueue(sse({ error: { message: e instanceof Error ? e.message : String(e), type: "c0mpute_error", code: 502 } }));
      }
      controller.enqueue(enc.encode("data: [DONE]\n\n"));
      controller.close();
    },
  });
}

/**
 * Pass an upstream SSE body through untouched while collecting the text, so
 * a streamed reply still teaches the router (verified once it ends).
 */
export function teeForLearning(body: ReadableStream<Uint8Array>, done: (text: string, finishReason: string | undefined, ms: number) => void): ReadableStream<Uint8Array> {
  const dec = new TextDecoder();
  let buf = "";
  let text = "";
  let finish: string | undefined;
  const t0 = Date.now();
  return body.pipeThrough(
    new TransformStream<Uint8Array, Uint8Array>({
      transform(chunk, controller) {
        controller.enqueue(chunk);
        buf += dec.decode(chunk, { stream: true });
        let nl: number;
        while ((nl = buf.indexOf("\n")) >= 0) {
          const line = buf.slice(0, nl).trim();
          buf = buf.slice(nl + 1);
          if (!line.startsWith("data:")) continue;
          const data = line.slice(5).trim();
          if (data === "[DONE]") continue;
          try {
            const j = JSON.parse(data) as { choices?: { delta?: { content?: string }; finish_reason?: string | null }[] };
            text += j.choices?.[0]?.delta?.content ?? "";
            finish = j.choices?.[0]?.finish_reason ?? finish;
          } catch {
            /* not JSON: ignore */
          }
        }
      },
      flush() {
        try {
          done(text, finish, Date.now() - t0);
        } catch {
          /* learning never breaks a reply */
        }
      },
    }),
  );
}

// ── the shared router ────────────────────────────────────────────────────

let _router: Router | null = null;

/** One router per process: learned stats are shared and persisted. */
export function sharedRouter(deps: { pool: () => Promise<ModelProfile[]>; store?: StatsStore; call: CallFn }): Router {
  return (_router ??= new Router({ call: deps.call, models: deps.pool, store: deps.store }));
}

export type RoutedChatArgs = {
  router: Router;
  upstream: Upstream;
  req: ChatRequest;
  route: RouteOptions;
  responseFormatJson?: boolean;
  signal?: AbortSignal;
};

/**
 * Answer one routed chat request.
 * - fast/standard + stream: the plan's model, streamed straight through
 *   (first token as fast as a direct request); the reply is still verified
 *   afterwards and teaches the router.
 * - everything else: the full plan (checks, escalation, critique, fusion),
 *   as JSON or as SSE once it finishes.
 */
export async function routedChat(a: RoutedChatArgs): Promise<Response> {
  // The caller's own messages: forwardChat adds the house prompt to model
  // calls, and its wording must not sway classification.
  const messages: Msg[] = a.req.messages.filter((m) => !isPoisonedTurn(m));
  const opts: RouteOptions = { ...a.route, maxTokens: a.req.max_tokens, temperature: a.req.temperature };
  const headers = { "cache-control": "no-store", "x-accel-buffering": "no" };

  if (a.req.stream && (opts.effort === "fast" || opts.effort === "standard")) {
    const d = await a.router.route(messages, opts, { responseFormatJson: a.responseFormatJson });
    const model = d.plan.primary;
    const upstream = await forwardChat(a.upstream, { ...a.req, model, stream: true }, a.signal);
    const type = upstream.headers.get("content-type") ?? "";
    if (!upstream.ok || !upstream.body || !type.includes("event-stream")) return upstream;
    const features = d.features;
    const body = teeForLearning(upstream.body, (text, finish, ms) => {
      a.router.observe(model, features, text, finish, { ms, outTokens: Math.ceil(text.length / 4) });
    });
    return new Response(body, {
      status: 200,
      headers: {
        ...headers,
        "content-type": "text/event-stream",
        [ROUTE_HEADER]: `pattern=single; model=${model}; streamed=true; reasons=${encodeURIComponent(d.plan.reasons.slice(-2).join(" | "))}`,
      },
    });
  }

  const call = infernetCall(a.upstream);
  const work = a.router.run(messages, opts, { responseFormatJson: a.responseFormatJson, signal: a.signal, call });
  if (a.req.stream) {
    return new Response(routedStream(work), { headers: { ...headers, "content-type": "text/event-stream" } });
  }
  const { text, model, receipt } = await work;
  return new Response(JSON.stringify(completionJson(text, model, receipt)), {
    headers: { ...headers, "content-type": "application/json", [ROUTE_HEADER]: routeHeader(receipt) },
  });
}

/** A dry run for POST /api/v1/route: classification, plan and alternatives, no model calls. */
export async function explainRoute(router: Router, messages: Msg[], route: RouteOptions, responseFormatJson = false) {
  const d = await router.route(messages, route, { responseFormatJson });
  return { router: "c0mpute-router/1", effort: route.effort, features: d.features, plan: d.plan, alternatives: d.alternatives };
}

export { classify };
