// How a resumable turn talks to the network, on whichever server runs it: the
// first time (from the completions route) or after a restart (recovery).

import { forwardChat, parseChatRequest, type ChatMessage, type Upstream } from "./completions";
import type { Router } from "@/lib/router";
import { pluginChatResponse } from "./plugin-http";
import { routeRequest, routedChat } from "./routed";
import { CONTINUE_PROMPT, type RunOpener } from "./runs";

/** The messages for a continued answer: history, the partial answer, then "continue". */
export function continuationMessages(messages: ChatMessage[], partial: string): ChatMessage[] {
  if (!partial.trim()) return messages;
  return [...messages, { role: "assistant", content: partial }, { role: "user", content: CONTINUE_PROMPT }];
}

export function chatRunOpener(upstreamFor: (subject: string | null) => Upstream, opts: { router?: () => Router | null } = {}): RunOpener {
  return async (req, signal, cont) => {
    const parsed = parseChatRequest(req.body);
    if (typeof parsed === "string") throw new Error(parsed);
    const upstream = upstreamFor(req.coinpaySub);
    if (req.withPlugins) {
      // A plugin turn runs again after a restart. Approvals are dropped then: an
      // approved destructive call may already have run, so it asks again.
      const approvals = cont ? [] : Array.isArray(req.body.approvals) ? req.body.approvals : [];
      const res = await pluginChatResponse({ userId: req.userId, upstream, req: parsed, approvals, events: req.body.plugin_events === true, signal });
      return res.body;
    }
    const messages = cont ? continuationMessages(parsed.messages, cont.partial) : parsed.messages;
    // Model router (DIP 0035) for "auto" turns; on any failure, the direct path.
    const router = opts.router?.();
    const route = router ? routeRequest(req.body) : null;
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
  };
}
