// How a resumable turn talks to the network, on whichever server runs it: the
// first time (from the completions route) or after a restart (recovery).

import { billingSubject } from "./billed-subject";
import { forwardChat, parseChatRequest, type ChatMessage, type ChatRequest, type Upstream } from "./completions";
import type { Router } from "@/lib/router";
import { pluginChatResponse } from "./plugin-http";
import { routeRequest, routedChat } from "./routed";
import { CONTINUE_PROMPT, type RunOpener } from "./runs";
import { groundQuestion, groundedMessages, lastUserText, needsWeb, type Grounding } from "./web-grounding";

/** The messages for a continued answer: history, the partial answer, then "continue". */
export function continuationMessages(messages: ChatMessage[], partial: string): ChatMessage[] {
  if (!partial.trim()) return messages;
  return [...messages, { role: "assistant", content: partial }, { role: "user", content: CONTINUE_PROMPT }];
}

/** The SSE event that hands the browser a grounded turn's sources (shown as cards). */
export const SOURCES_OBJECT = "c0mpute.sources";

const enc = new TextEncoder();
const sse = (o: unknown) => `data: ${JSON.stringify(o)}\n\n`;
const status = (message: string) => `: status ${JSON.stringify({ state: "grounding", stage: "grounding", message })}\n\n`;

export type OpenerDeps = {
  router?: () => Router | null;
  /** Web grounding (tests swap it). */
  ground?: (question: string, opts: { signal: AbortSignal; onStatus: (m: string) => void }) => Promise<Grounding | null>;
};

export function chatRunOpener(upstreamFor: (subject: string | null) => Upstream, opts: OpenerDeps = {}): RunOpener {
  const ground = opts.ground ?? ((q, o) => groundQuestion(q, o));
  return async (req, signal, cont) => {
    const parsed = parseChatRequest(req.body);
    if (typeof parsed === "string") throw new Error(parsed);
    // The CoinPay sub when linked, else the signed `c0mpute:<id>`: without it
    // an unlinked person was metered as our server's IP, one shared allowance.
    const upstream = upstreamFor(billingSubject({ id: req.userId, coinpay_sub: req.coinpaySub }));
    if (req.withPlugins) {
      // A plugin turn runs again after a restart. Approvals are dropped then: an
      // approved destructive call may already have run, so it asks again.
      const approvals = cont ? [] : Array.isArray(req.body.approvals) ? req.body.approvals : [];
      const res = await pluginChatResponse({ userId: req.userId, upstream, req: parsed, approvals, events: req.body.plugin_events === true, signal });
      return res.body;
    }
    const messages = cont ? continuationMessages(parsed.messages, cont.partial) : parsed.messages;
    const web = req.body.web === true;
    // Nodes default to 512 output tokens, which cut a full answer off mid-table
    // (2026-11-22); the web chat asks for room unless the caller set a limit.
    const room = (n: number): ChatRequest => (web && parsed.max_tokens === undefined ? { ...parsed, max_tokens: n } : parsed);
    const open = (msgs: ChatMessage[], grounded = false) => openAnswer(upstream, opts.router?.() ?? null, req.body, room(grounded ? 1600 : 1200), msgs, signal);

    // The web chat asks for grounding (`web: true`); API clients keep the plain path.
    const question = lastUserText(parsed.messages);
    // A prompt command (/analyze, /runway...) works from the person's data and plugins, not a web search.
    if (!web || parsed.command || !needsWeb(question)) return open(messages);

    // Stream at once: where the search is, then the sources, then the answer.
    let inner: ReadableStreamDefaultReader<Uint8Array> | null = null;
    return new ReadableStream<Uint8Array>({
      async start(ctrl) {
        const push = (s: string) => {
          try {
            ctrl.enqueue(enc.encode(s));
          } catch {}
        };
        const g = await ground(question, { signal, onStatus: (m) => push(status(m)) }).catch(() => null);
        if (signal.aborted) return void ctrl.close();
        if (g) push(sse({ object: SOURCES_OBJECT, query: g.query, sources: g.sources }));
        push(status(g ? `asking a node, with ${g.sources.length} sources…` : "asking a node…"));
        let body: ReadableStream<Uint8Array> | null;
        try {
          body = await open(g ? groundedMessages(messages, g) : messages, !!g);
        } catch (e) {
          push(sse({ error: { message: e instanceof Error ? e.message : String(e), type: "c0mpute_error", code: 502 } }) + "data: [DONE]\n\n");
          return void ctrl.close();
        }
        if (!body) return void ctrl.close();
        inner = body.getReader();
        try {
          for (;;) {
            const { done, value } = await inner.read();
            if (done) break;
            if (value?.byteLength) ctrl.enqueue(value);
          }
          ctrl.close();
        } catch (e) {
          ctrl.error(e);
        }
      },
      cancel() {
        void inner?.cancel().catch(() => {});
      },
    });
  };
}

/** One answer from the network: through the model router for "auto" turns, else (or on any router failure) direct. */
async function openAnswer(
  upstream: Upstream,
  router: Router | null,
  body: Record<string, unknown>,
  parsed: ChatRequest,
  messages: ChatMessage[],
  signal: AbortSignal,
): Promise<ReadableStream<Uint8Array> | null> {
  const route = router ? routeRequest(body) : null;
  if (router && route) {
    try {
      const res = await routedChat({ router, upstream, req: { ...parsed, messages, stream: true }, route, signal });
      if (res.ok && res.body && (res.headers.get("content-type") ?? "").includes("event-stream")) return res.body;
      await res.body?.cancel().catch(() => {});
    } catch (e) {
      if (signal.aborted) throw e;
    }
  }
  const up = await forwardChat(upstream, { ...parsed, messages }, signal);
  const type = up.headers.get("content-type") ?? "";
  if (!up.ok || !type.includes("event-stream")) {
    const text = await up.text().catch(() => "");
    let message = `the c0mpute network is restarting or busy (upstream HTTP ${up.status}); try again in a minute`;
    try {
      message = JSON.parse(text).error?.message ?? message;
    } catch {}
    throw new Error(message);
  }
  return up.body;
}
