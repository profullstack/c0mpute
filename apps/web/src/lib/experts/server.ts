// Server wiring for the Experts store: SQLite Cloud, CoinPay checkout (the
// c0mpute.com business, same keys as the gallery), the plugin loop limited to
// each expert's allowed MCP servers, the DIP-0036 labeler pass at boot, and
// the one HTTP handler both /v1/chat/completions (model "expert:<slug>") and
// /api/v1/experts/<slug>/ask go through.

import { CoinPay } from "@profullstack/coinpay-onboard";
import type { ChatRequest } from "../chat/completions";
import { labelerFromEnv, moderatePush } from "../chat/moderation";
import { hasEnabledPlugins, runPluginChat } from "../chat/plugins";
import { cloudSql, env, infernetFor, json, pluginStore, rateLimit, siteUrl } from "../chat/server";
import { coinpayBase } from "../gallery/server";
import { expertBySlug, experts, moderateCatalog } from "./catalog";
import { completionBody, completionStream, expertHeaders, expertRequest, expertText, forwardExpert, pluginAllowed, stripThinking } from "./chat";
import { expertsConfig } from "./config";
import type { ExpertManifest } from "./manifest";
import { createExpertsService, ExpertsError, type Asker, type CheckoutOpener, type ExpertsService } from "./service";
import { createExpertsStore } from "./store";

const checkout: CheckoutOpener = async (o) => {
  const key = env("COINPAY_API_KEY");
  if (!key) throw new Error("COINPAY_API_KEY is not set");
  const cp = new CoinPay({ token: key, base: coinpayBase() });
  const s = await cp.createCheckoutSession({ ...o, property: "c0mpute.com" });
  return { id: s.id, url: s.url };
};

let _service: ExpertsService | null = null;
export const expertsService = () =>
  (_service ??= createExpertsService({
    store: createExpertsStore(cloudSql),
    catalog: () => experts(),
    checkout: env("COINPAY_API_KEY") ? checkout : undefined,
    site: siteUrl(),
  }));

/** An ExpertsError (or anything) as the API's error envelope. */
export function expertsErrorResponse(e: unknown, headers: Record<string, string> = {}) {
  if (e instanceof ExpertsError) return json({ error: { message: e.message, type: "c0mpute_experts", code: e.status, reason: e.code, ...e.extra } }, e.status, headers);
  console.error("experts:", e);
  return json({ error: { message: e instanceof Error ? e.message : String(e), type: "c0mpute_experts", code: 500 } }, 500, headers);
}

type User = Asker & { token_kind?: string };

/**
 * Plugins the person connected AND the expert allows (matched by host).
 * Returns null when there are none, so the caller takes the plain path.
 */
async function pluginRun(user: User, m: ExpertManifest, req: ChatRequest, signal?: AbortSignal): Promise<{ text: string; model: string } | null> {
  if (!m.tools.mcp.length || !(await hasEnabledPlugins(user.id))) return null;
  const base = pluginStore();
  const list = (await base.list(user.id)).filter((p) => p.enabled && pluginAllowed(m, p.url));
  if (!list.length) return null;
  const store = { ...base, list: async (u: string) => (await base.list(u)).filter((p) => pluginAllowed(m, p.url)) };
  const r = await runPluginChat({ userId: user.id, upstream: infernetFor(user.coinpay_sub), req: expertRequest(m, { ...req, stream: false }, m.model.prefer[0]), emit: () => {}, store, signal });
  return { text: stripThinking(r.text), model: r.model };
}

/**
 * One expert turn for a signed-in user: gate (trial / plan / paywall), then the
 * plugin loop or the model failover. JSON or SSE in OpenAI's shape, with
 * x-c0mpute-expert (+ x-c0mpute-disclaimer) headers and x_c0mpute_expert in the body.
 */
export async function expertChatResponse(o: { user: User; slug: string; req: ChatRequest; signal?: AbortSignal }): Promise<Response> {
  const m = expertBySlug(o.slug);
  if (!m) return json({ error: { message: `no expert "${o.slug}": see GET /api/v1/experts`, type: "c0mpute_experts", code: 404 } }, 404);
  const rl = rateLimit(`expert:${o.user.id}`, expertsConfig().asksPerHour);
  if (!rl.ok) return json({ error: { message: "too many expert questions this hour", type: "c0mpute_experts", code: 429 } }, 429, { "retry-after": String(rl.retryAfter) });
  try {
    await expertsService().gate(o.user, m);
  } catch (e) {
    return expertsErrorResponse(e, expertHeaders(m));
  }

  try {
    const viaPlugins = await pluginRun(o.user, m, o.req, o.signal);
    if (viaPlugins) {
      const headers = { ...expertHeaders(m, viaPlugins.model), "cache-control": "no-store" };
      if (o.req.stream) return new Response(completionStream(m, viaPlugins.text, viaPlugins.model), { headers: { ...headers, "content-type": "text/event-stream", "x-accel-buffering": "no" } });
      return json(completionBody(m, viaPlugins.text, viaPlugins.model), 200, headers);
    }
  } catch (e) {
    if (o.signal?.aborted) throw e;
    console.warn(`experts: plugin path failed for ${m.slug}, answering without tools:`, e instanceof Error ? e.message : e);
  }

  let r: Awaited<ReturnType<typeof forwardExpert>>;
  try {
    r = await forwardExpert(infernetFor(o.user.coinpay_sub), m, o.req, o.signal);
  } catch (e) {
    return json({ error: { message: e instanceof Error ? e.message : String(e), type: "c0mpute_experts", code: 503 } }, 503, { "retry-after": "30", ...expertHeaders(m) });
  }
  const type = r.res.headers.get("content-type") ?? "";
  if (!type.includes("json") && !type.includes("event-stream")) {
    await r.res.body?.cancel().catch(() => {});
    return json({ error: { message: `the c0mpute network is restarting or busy (upstream HTTP ${r.res.status}); try again in a minute`, type: "c0mpute_experts", code: 503 } }, 503, { "retry-after": "30", ...expertHeaders(m) });
  }
  if (!r.res.ok || o.req.stream) {
    return new Response(r.res.body, {
      status: r.res.status,
      headers: { "content-type": type || "application/json", "cache-control": "no-store", "x-accel-buffering": "no", ...expertHeaders(m, r.model) },
    });
  }
  const body = (await r.res.json().catch(() => null)) as { choices?: { message?: { content?: string } }[]; model?: string } | null;
  const text = stripThinking(body?.choices?.[0]?.message?.content ?? "");
  const out = { ...(body ?? {}), ...completionBody(m, text, body?.model ?? r.model), ...(body && "usage" in body ? { usage: (body as { usage: unknown }).usage } : {}) };
  return json(out, 200, expertHeaders(m, r.model));
}

/**
 * The answer as text (MCP ask_expert): the same gate, plugins and failover.
 * Throws ExpertsError for the paywall / CoinPay cases.
 */
export async function expertAnswer(user: User, slug: string, req: ChatRequest, signal?: AbortSignal): Promise<{ text: string; model: string; expert: ExpertManifest }> {
  const m = expertBySlug(slug);
  if (!m) throw new ExpertsError(`no expert "${slug}": call list_experts`, 404, "unknown_expert");
  const rl = rateLimit(`expert:${user.id}`, expertsConfig().asksPerHour);
  if (!rl.ok) throw new ExpertsError("too many expert questions this hour", 429, "rate_limited");
  await expertsService().gate(user, m);
  const viaPlugins = await pluginRun(user, m, req, signal).catch(() => null);
  if (viaPlugins) return { ...viaPlugins, expert: m };
  const r = await expertText(infernetFor(user.coinpay_sub), m, req, signal);
  return { ...r, expert: m };
}

let moderated = false;
/** Once per process, a few minutes after boot: run the catalog past the DIP-0036 labeler. */
export function startExpertsModeration() {
  if (moderated) return;
  moderated = true;
  const t = setTimeout(() => {
    const cfg = labelerFromEnv();
    if (!cfg) return;
    moderateCatalog(async (m) => {
      const v = await moderatePush([{ path: `${m.slug}.manifest.json`, type: "application/json", text: JSON.stringify(m) }], cfg);
      return v.verdict === "refused" ? { verdict: "refused", message: v.message } : { verdict: v.verdict === "ok" ? "ok" : "unchecked" };
    }).catch((e) => console.error("experts: moderation pass failed", e));
  }, 180_000);
  (t as { unref?: () => void }).unref?.();
}
