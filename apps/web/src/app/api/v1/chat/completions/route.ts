import { forwardChat, parseChatRequest } from "@/lib/chat/completions";
import { hasEnabledPlugins } from "@/lib/chat/plugins";
import { pluginChatResponse } from "@/lib/chat/plugin-http";
import { runManager } from "@/lib/chat/run-manager";
import { RunLimitError, SSE_HEADERS } from "@/lib/chat/runs";
import { routeRequest, routedChat, routerEnabled } from "@/lib/chat/routed";
import { toolChatResponse } from "@/lib/chat/tools-http";
import { houseLaneChat, wantsHouseLane } from "@/lib/chat/house-lane";
import { balanceFor, currentUser, env, infernetFor, json, modelRouter, rateLimit, sameOrigin, siteUrl, unauthorized } from "@/lib/chat/server";
import { recordUsage } from "@/lib/leaderboard/server";
import { expertSlugOf } from "@/lib/experts/chat";
import { expertChatResponse } from "@/lib/experts/server";
import { billingSubject } from "@/lib/chat/billed-subject";

export const dynamic = "force-dynamic";
export const maxDuration = 300;

const err = (status: number, message: string, headers: Record<string, string> = {}) =>
  json({ error: { message, type: "c0mpute_error", code: status } }, status, headers);

/**
 * OpenAI-compatible chat completions on the c0mpute network.
 *   Authorization: Bearer c0k_...   (or the c0mpute.com session cookie)
 *   POST { model?, messages, stream?, temperature?, max_tokens?, tools?, tool_choice?, parallel_tool_calls?,
 *          response_format?, stream_options?, plugins?: false, approvals?: string[], plugin_events?: true }
 *
 * `tools`: OpenAI function calling. Replies carry message.tool_calls and
 * finish_reason "tool_calls" (streamed as delta.tool_calls); role "tool"
 * results and assistant tool_calls are accepted in history. Only
 * tool-capable models take them (lib/chat/tools.ts); others get a 400.
 *
 * `model`: "expert:<slug>" asks a c0mpute Expert (GET /api/v1/experts; trial,
 * then subscription). A model id goes straight to that model. "auto" (or absent),
 * "auto:fast" | "auto:standard" | "auto:high" | "auto:max" goes through the
 * model router; `router: { effort?, max_cost_usd?, max_latency_ms? }` refines
 * it. Routed answers carry `x-c0mpute-route` and (JSON / last SSE chunk) the
 * full `x_c0mpute_route` receipt. POST /api/v1/route explains a plan for free.
 * With chat plugins on, the stream also carries `{"object":"c0mpute.plugin_event","event":{...}}`
 * chunks (tool calls, results, approvals) when the request sets plugin_events: true.
 *
 * `resumable: true` (with stream): the turn runs to the end even if the
 * connection drops; the response carries `x-c0mpute-run: <id>` and a first
 * `{"object":"c0mpute.run","id"}` chunk, GET /api/v1/chat/runs/<id> replays it
 * and DELETE stops it (closing the connection no longer does).
 */
export async function POST(req: Request) {
  // OAuth keys with the "inference" scope (apps acting for a person) may chat too.
  const user = await currentUser(req, { scope: "inference" });
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return err(403, "cross-site request refused");
  const rl = rateLimit(user.id);
  if (!rl.ok) return err(429, "rate limit: too many chats this hour", { "retry-after": String(rl.retryAfter) });

  const raw = (await req.json().catch(() => null)) as Record<string, unknown> | null;
  const parsed = parseChatRequest(raw);
  if (typeof parsed === "string") return err(400, parsed);

  // House lane (lib/chat/house-lane.ts): Profullstack-operated node only,
  // paid only, nothing stored, no fallback. Checked before every other path
  // so no plugin, run store, router or open-network route ever sees it.
  if (wantsHouseLane(req, raw)) {
    recordUsage(user.id, user.token_kind === "session" ? "chat" : "api", "house");
    return houseLaneChat({
      subject: user.coinpay_sub,
      req: parsed,
      signal: req.signal,
      deps: { fetch, env, balance: (sub) => balanceFor(sub), topUpUrl: `${siteUrl()}/checkout` },
    });
  }

  // One count for the public leaderboard: the surface and the model name, nothing else.
  recordUsage(user.id, user.token_kind === "session" ? "chat" : "api", typeof raw?.model === "string" ? raw.model : "auto");
  // An app-held (OAuth) key gets plain inference: never the person's chat
  // plugins (their connected tools) nor a stored, resumable run.
  const appKey = user.token_kind === "oauth";

  // An Expert (model "expert:<slug>"): the signed manifest applied to this
  // turn, behind the Experts trial / subscription gate (lib/experts).
  const expert = expertSlugOf(raw?.model);
  if (expert) {
    if (parsed.tooling) return err(400, "experts do not take client tools; drop `tools` or use a model id");
    return expertChatResponse({ user, slug: expert, req: parsed, signal: req.signal });
  }

  // OpenAI tool calling (`tools`, tool_choice not "none"): the client runs its
  // own tool loop, so server-side plugins, resumable runs and the multi-step
  // router stay out of it. Only tool-capable models take it; never dropped.
  if (parsed.tooling) {
    return toolChatResponse({ upstream: infernetFor(billingSubject(user)), req: parsed, rawModel: raw?.model, signal: req.signal });
  }

  // Chat plugins: with any plugin on (and not `plugins: false`), the server
  // runs the tool loop; each model step is billed like a normal turn.
  const approvals = Array.isArray(raw?.approvals) ? raw.approvals : [];
  const withPlugins = !appKey && raw?.plugins !== false && (approvals.length > 0 || (await hasEnabledPlugins(user.id)));

  if (!appKey && raw?.resumable === true && parsed.stream && raw) {
    try {
      // Kept (sealed) with the run so any server can continue it after a deploy.
      const runs = runManager();
      const run = await runs.start({ userId: user.id, coinpaySub: user.coinpay_sub ?? null, body: raw, withPlugins });
      return new Response(runs.stream(run.id), { headers: { ...SSE_HEADERS, "x-c0mpute-run": run.id } });
    } catch (e) {
      if (e instanceof RunLimitError) return err(429, e.message, { "retry-after": "10" });
      throw e;
    }
  }

  if (withPlugins) {
    return pluginChatResponse({ userId: user.id, upstream: infernetFor(billingSubject(user)), req: parsed, approvals, events: raw?.plugin_events === true, signal: req.signal });
  }

  // Model router (DIP 0035): `model` "auto", "auto:<effort>" or absent picks
  // the model(s) and pattern per request. Any failure here falls back to the
  // direct path below, so routing can never take chat down.
  const route = routerEnabled(env) ? routeRequest(raw) : null;
  if (route) {
    try {
      const rf = (raw?.response_format ?? null) as { type?: string } | null;
      return await routedChat({
        router: modelRouter(),
        upstream: infernetFor(billingSubject(user)),
        req: parsed,
        route,
        responseFormatJson: rf?.type === "json_object" || rf?.type === "json_schema",
        signal: req.signal,
      });
    } catch (e) {
      if (req.signal.aborted) throw e;
      console.warn("router: falling back to direct", e instanceof Error ? e.message : e);
    }
  }

  let upstream: Response;
  try {
    // Billed to this person's compute balance when they have one; otherwise
    // Infernet serves it on its free tier exactly as before.
    upstream = await forwardChat(infernetFor(billingSubject(user)), parsed, req.signal);
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
