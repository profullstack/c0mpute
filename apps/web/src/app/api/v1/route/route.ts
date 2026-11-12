import { parseChatRequest } from "@/lib/chat/completions";
import { explainRoute, routeRequest } from "@/lib/chat/routed";
import { clientIp, json, modelRouter, rateLimit } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * Explain a routing decision without running it (free, public):
 *   POST { messages, model?: "auto" | "auto:<effort>", router?: { effort, max_cost_usd, max_latency_ms }, response_format? }
 * Returns how the request was classified, the plan the router would run
 * (pattern, models, judge, fallbacks, expected quality/cost/latency, and
 * why), and the next-best alternatives.
 */
export async function POST(req: Request) {
  const rl = rateLimit(`route:${clientIp(req)}`, 120);
  if (!rl.ok) return json({ error: { message: "rate limit", code: 429 } }, 429, { "retry-after": String(rl.retryAfter) });
  const raw = (await req.json().catch(() => null)) as Record<string, unknown> | null;
  const parsed = parseChatRequest(raw);
  if (typeof parsed === "string") return json({ error: { message: parsed, code: 400 } }, 400);
  const route = routeRequest({ ...(raw ?? {}), model: raw?.model ?? "auto" });
  if (!route) return json({ error: { message: "model names a specific model; use auto or auto:<effort> to route", code: 400 } }, 400);
  const rf = (raw?.response_format ?? null) as { type?: string } | null;
  try {
    return json(await explainRoute(modelRouter(), parsed.messages, { ...route, maxTokens: parsed.max_tokens }, rf?.type === "json_object" || rf?.type === "json_schema"));
  } catch (e) {
    return json({ error: { message: e instanceof Error ? e.message : String(e), code: 503 } }, 503);
  }
}
