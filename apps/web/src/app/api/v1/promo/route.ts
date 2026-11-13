import { gateGeneration } from "@/lib/chat/adult";
import { parsePublicPromoRequest } from "@/lib/chat/promo-guard";
import { currentUser, infernetFor, json, rateLimit, recordJob, sameOrigin, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

const err = (status: number, message: string, headers: Record<string, string> = {}) =>
  json({ error: { message, type: "c0mpute_error", code: status } }, status, headers);

/**
 * Start a promo video of a website on the c0mpute network.
 *   POST { url, style?: pixar|anime|claymation|flat|watercolor|comic|cinematic, vertical?, brief? }
 *     ->  202 { id, poll }
 * Poll GET /api/v1/promo/<id> for per-stage progress and storyboard frames
 * (GET .../frame/<n>); when status is "done", GET .../video (MP4) and
 * .../captions (SRT). Takes 5 to 15 minutes. Billed to the person's compute
 * balance for the work it took, or one of Infernet's free daily queries.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return err(403, "cross-site request refused");
  const rl = rateLimit(`promo:${user.id}`, 6);
  if (!rl.ok) return err(429, "rate limit: too many promos this hour", { "retry-after": String(rl.retryAfter) });
  const parsed = parsePublicPromoRequest(await req.json().catch(() => null));
  if (typeof parsed === "string") return err(400, parsed);
  // Promos are always SFW (never adult mode): the brief steers the
  // storyboard, so it passes the same content policy as an image prompt.
  const gate = gateGeneration(user, { prompt: parsed.brief || parsed.url });
  if (!gate.ok) return json({ error: { message: gate.message, type: "content_policy", code: gate.code } }, gate.status);
  let upstream: Response;
  try {
    upstream = await infernetFor(user.coinpay_sub)("/api/chat/promo", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(parsed),
      signal: AbortSignal.timeout(20_000),
    });
  } catch (e) {
    return err(502, `network unreachable: ${e instanceof Error ? e.message : e}`);
  }
  const body = (await upstream.json().catch(() => null)) as Record<string, unknown> | null;
  if (upstream.status !== 202 || typeof body?.id !== "string") {
    const message = typeof body?.error === "string" ? body.error : `promo could not start (upstream HTTP ${upstream.status})`;
    return err(upstream.status >= 400 ? upstream.status : 502, message);
  }
  await recordJob(user.id, "promo", body.id);
  return json({ id: body.id, status: "pending", poll: `/api/v1/promo/${body.id}` }, 202, { "cache-control": "no-store" });
}
