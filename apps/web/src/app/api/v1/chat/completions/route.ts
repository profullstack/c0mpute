import { forwardChat, parseChatRequest } from "@/lib/chat/completions";
import { hasEnabledPlugins } from "@/lib/chat/plugins";
import { pluginChatResponse } from "@/lib/chat/plugin-http";
import { RunLimitError, SSE_HEADERS, runStream, startRun } from "@/lib/chat/runs";
import { currentUser, infernetFor, json, rateLimit, sameOrigin, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";
export const maxDuration = 300;

const err = (status: number, message: string, headers: Record<string, string> = {}) =>
  json({ error: { message, type: "c0mpute_error", code: status } }, status, headers);

/**
 * OpenAI-compatible chat completions on the c0mpute network.
 *   Authorization: Bearer c0k_...   (or the c0mpute.com session cookie)
 *   POST { model?, messages, stream?, temperature?, max_tokens?, plugins?: false, approvals?: string[], plugin_events?: true }
 * With chat plugins on, the stream also carries `{"object":"c0mpute.plugin_event","event":{...}}`
 * chunks (tool calls, results, approvals) when the request sets plugin_events: true.
 *
 * `resumable: true` (with stream): the turn runs to the end even if the
 * connection drops; the response carries `x-c0mpute-run: <id>` and a first
 * `{"object":"c0mpute.run","id"}` chunk, GET /api/v1/chat/runs/<id> replays it
 * and DELETE stops it (closing the connection no longer does).
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return err(403, "cross-site request refused");
  const rl = rateLimit(user.id);
  if (!rl.ok) return err(429, "rate limit: too many chats this hour", { "retry-after": String(rl.retryAfter) });

  const raw = (await req.json().catch(() => null)) as Record<string, unknown> | null;
  const parsed = parseChatRequest(raw);
  if (typeof parsed === "string") return err(400, parsed);

  // Chat plugins: with any plugin on (and not `plugins: false`), the server
  // runs the tool loop; each model step is billed like a normal turn.
  const approvals = Array.isArray(raw?.approvals) ? raw.approvals : [];
  const withPlugins = raw?.plugins !== false && (approvals.length > 0 || (await hasEnabledPlugins(user.id)));

  if (raw?.resumable === true && parsed.stream) {
    try {
      const run = await startRun(user.id, async (signal) => {
        if (withPlugins) {
          const res = await pluginChatResponse({ userId: user.id, upstream: infernetFor(user.coinpay_sub), req: parsed, approvals, events: raw?.plugin_events === true, signal });
          return res.body;
        }
        const up = await forwardChat(infernetFor(user.coinpay_sub), parsed, signal);
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
      });
      return new Response(runStream(run), { headers: { ...SSE_HEADERS, "x-c0mpute-run": run.id } });
    } catch (e) {
      if (e instanceof RunLimitError) return err(429, e.message, { "retry-after": "10" });
      throw e;
    }
  }

  if (withPlugins) {
    return pluginChatResponse({ userId: user.id, upstream: infernetFor(user.coinpay_sub), req: parsed, approvals, events: raw?.plugin_events === true, signal: req.signal });
  }

  let upstream: Response;
  try {
    // Billed to this person's compute balance when they have one; otherwise
    // Infernet serves it on its free tier exactly as before.
    upstream = await forwardChat(infernetFor(user.coinpay_sub), parsed, req.signal);
  } catch (e) {
    return err(502, `network unreachable: ${e instanceof Error ? e.message : e}`);
  }
  const type = upstream.headers.get("content-type") ?? "";
  if (!upstream.ok && !type.includes("json")) {
    // An nginx/HTML page (Infernet restarting, a deploy): never hand HTML to an API client.
    await upstream.body?.cancel().catch(() => {});
    return err(503, `the c0mpute network is restarting or busy (upstream HTTP ${upstream.status}); try again in a minute`, {
      "retry-after": "30",
    });
  }
  // Pass Infernet's body straight through: JSON, OpenAI SSE chunks, or its error envelope.
  return new Response(upstream.body, {
    status: upstream.status,
    headers: {
      "content-type": upstream.headers.get("content-type") ?? "application/json",
      "cache-control": "no-store",
      "x-accel-buffering": "no",
    },
  });
}
