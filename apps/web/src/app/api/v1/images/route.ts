import { gateGeneration } from "@/lib/chat/adult";
import { parseImageRequest } from "@/lib/chat/images";
import { balanceFor, currentUser, infernetFor, json, rateLimit, recordJob, sameOrigin, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

const err = (status: number, message: string, headers: Record<string, string> = {}) =>
  json({ error: { message, type: "c0mpute_error", code: status } }, status, headers);

/**
 * Start an image on the c0mpute network (Z-Image-Turbo on CPU workers).
 *   POST { prompt, size?: "landscape"|"square"|"portrait", adult?: true }  ->  202 { id, poll }
 * Poll GET /api/v1/images/<id> until it returns image/png (~2 minutes).
 * Billed to the person's compute balance when they have one (flat per image);
 * otherwise it spends one of Infernet's free daily queries.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return err(403, "cross-site request refused");
  const rl = rateLimit(`img:${user.id}`, 20);
  if (!rl.ok) return err(429, "rate limit: too many images this hour", { "retry-after": String(rl.retryAfter) });
  const parsed = parseImageRequest(await req.json().catch(() => null));
  if (typeof parsed === "string") return err(400, parsed);
  // Content policy first: the child-safety guard (always on), explicit
  // prompts only in adult mode, and adult mode only for a verified 18+
  // account with a paid balance. The network's gateway and the worker check
  // again; a refusal here never leaves c0mpute.com.
  const gate = gateGeneration(user, { prompt: parsed.prompt, adult: parsed.adult === true }, parsed.adult ? await balanceFor(user.coinpay_sub) : null);
  if (!gate.ok) return json({ error: { message: gate.message, type: "content_policy", code: gate.code } }, gate.status);

  let upstream: Response;
  try {
    upstream = await infernetFor(user.coinpay_sub)("/api/chat/image", {
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
    const message = typeof body?.error === "string" ? body.error : `image could not start (upstream HTTP ${upstream.status})`;
    return err(upstream.status >= 400 ? upstream.status : 502, message);
  }
  await recordJob(user.id, "image", body.id);
  return json({ id: body.id, status: "pending", poll: `/api/v1/images/${body.id}` }, 202, { "cache-control": "no-store" });
}
