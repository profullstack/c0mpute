// How a resumable turn talks to the network, on whichever server runs it: the
// first time (from the completions route) or after a restart (recovery).

import { forwardChat, parseChatRequest, type ChatMessage, type Upstream } from "./completions";
import { pluginChatResponse } from "./plugin-http";
import { CONTINUE_PROMPT, type RunOpener } from "./runs";

/** The messages for a continued answer: history, the partial answer, then "continue". */
export function continuationMessages(messages: ChatMessage[], partial: string): ChatMessage[] {
  if (!partial.trim()) return messages;
  return [...messages, { role: "assistant", content: partial }, { role: "user", content: CONTINUE_PROMPT }];
}

export function chatRunOpener(upstreamFor: (subject: string | null) => Upstream): RunOpener {
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
