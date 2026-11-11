// The HTTP shape of a plugin-assisted chat turn: OpenAI-compatible JSON or SSE,
// with the tool steps as extra `c0mpute.plugin_event` chunks.
import { randomBytes } from "node:crypto";
import type { ChatRequest, Upstream } from "./completions";
import { runPluginChat } from "./plugins";
import { PLUGIN_EVENT_OBJECT, type PluginEvent } from "./plugin-shared";
import { json } from "./server";

export function pluginChatResponse(opts: { userId: string; upstream: Upstream; req: ChatRequest; approvals: unknown[]; events?: boolean; signal?: AbortSignal }) {
  const id = `chatcmpl-${randomBytes(12).toString("hex")}`;
  const created = Math.floor(Date.now() / 1000);
  if (!opts.req.stream) {
    return (async () => {
      const events: PluginEvent[] = [];
      try {
        const r = await runPluginChat({ ...opts, emit: (e) => events.push(e) });
        if (!opts.events) r.text += approvalNote(events);
        return json({
          id,
          object: "chat.completion",
          created,
          model: r.model,
          choices: [{ index: 0, message: { role: "assistant", content: r.text }, finish_reason: r.stopped === "approval" ? "approval_needed" : "stop" }],
          ...(opts.events ? { plugin_events: events } : {}),
        });
      } catch (e) {
        return json({ error: { message: e instanceof Error ? e.message : String(e), type: "c0mpute_error", code: 502 }, plugin_events: events }, 502);
      }
    })();
  }
  const enc = new TextEncoder();
  // The client hanging up (Stop, Esc, Ctrl+C) cancels the stream; abort the
  // tool loop with it instead of finishing a turn nobody will read.
  const stop = new AbortController();
  const signal = opts.signal ? AbortSignal.any([opts.signal, stop.signal]) : stop.signal;
  const stream = new ReadableStream({
    cancel() {
      stop.abort();
    },
    async start(ctrl) {
      const send = (o: unknown) => {
        try {
          ctrl.enqueue(enc.encode(`data: ${JSON.stringify(o)}\n\n`));
        } catch {}
      };
      // Keep nginx and the browser from timing out while a slow tool runs.
      const ping = setInterval(() => {
        try {
          ctrl.enqueue(enc.encode(": ping\n\n"));
        } catch {}
      }, 15_000);
      try {
        // Plugin event chunks have no `choices`, so only clients that asked
        // (plugin_events: true: the web chat, CLI, desktop) get them.
        const asked: PluginEvent[] = [];
        const r = await runPluginChat({
          ...opts,
          signal,
          emit: (event) => (opts.events ? send({ object: PLUGIN_EVENT_OBJECT, event }) : asked.push(event)),
        });
        if (!opts.events) r.text += approvalNote(asked);
        const chunk = (delta: Record<string, unknown>, finish: string | null) => ({
          id,
          object: "chat.completion.chunk",
          created,
          model: r.model,
          choices: [{ index: 0, delta, finish_reason: finish }],
        });
        send(chunk({ role: "assistant", content: r.text }, null));
        send(chunk({}, r.stopped === "approval" ? "approval_needed" : "stop"));
      } catch (e) {
        send({ error: { message: e instanceof Error ? e.message : String(e), type: "c0mpute_error", code: 502 } });
      } finally {
        clearInterval(ping);
        try {
          ctrl.enqueue(enc.encode("data: [DONE]\n\n"));
          ctrl.close();
        } catch {}
      }
    },
  });
  return new Response(stream, {
    headers: { "content-type": "text/event-stream", "cache-control": "no-store", "x-accel-buffering": "no" },
  });
}

/** For clients that do not read plugin events: say which call is waiting. */
function approvalNote(events: PluginEvent[]): string {
  const waiting = events.filter((e) => e.type === "approval_needed");
  if (!waiting.length) return "";
  return `\n\n(${waiting.map((e) => `${e.plugin} · ${e.tool}`).join(", ")} needs your approval: run this chat at https://c0mpute.com/chat or with \`c0mpute chat\`.)`;
}
