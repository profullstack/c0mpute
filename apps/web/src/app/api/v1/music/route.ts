import { parseMusicRequest } from "@/lib/chat/music";
import { currentUser, infernetFor, json, rateLimit, sameOrigin, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

const err = (status: number, message: string, headers: Record<string, string> = {}) =>
  json({ error: { message, type: "c0mpute_error", code: status } }, status, headers);

/**
 * Start a song on the c0mpute network (ACE-Step 1.5 on c0mpute workers).
 *   POST { prompt, lyrics?, duration?: 10..120 }  ->  202 { id, poll }
 * Poll GET /api/v1/music/<id> until it returns audio (a few minutes).
 * No lyrics = instrumental. Billed to the person's compute balance when they
 * have one (flat per track); otherwise it spends one of Infernet's free
 * daily queries.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return err(403, "cross-site request refused");
  const rl = rateLimit(`music:${user.id}`, 10);
  if (!rl.ok) return err(429, "rate limit: too many songs this hour", { "retry-after": String(rl.retryAfter) });
  const parsed = parseMusicRequest(await req.json().catch(() => null));
  if (typeof parsed === "string") return err(400, parsed);

  let upstream: Response;
  try {
    upstream = await infernetFor(user.coinpay_sub)("/api/chat/music", {
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
    const message = typeof body?.error === "string" ? body.error : `song could not start (upstream HTTP ${upstream.status})`;
    return err(upstream.status >= 400 ? upstream.status : 502, message);
  }
  return json({ id: body.id, status: "pending", poll: `/api/v1/music/${body.id}` }, 202, { "cache-control": "no-store" });
}
