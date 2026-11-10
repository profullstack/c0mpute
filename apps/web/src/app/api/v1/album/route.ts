import { parseAlbumRequest } from "@/lib/chat/album";
import { currentUser, infernetFor, json, rateLimit, sameOrigin, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

const err = (status: number, message: string, headers: Record<string, string> = {}) =>
  json({ error: { message, type: "c0mpute_error", code: status } }, status, headers);

/**
 * Start an album on the c0mpute network.
 *   POST { prompt, tracks?: 2..12 (default 8), duration?: 10..180 }  ->  202 { id, poll, tracks }
 * Infernet names a band, an album and its songs, writes their lyrics, draws
 * a cover and renders every track (ACE-Step 1.5 on c0mpute workers). Poll
 * GET /api/v1/album/<id> for the plan and each track; a finished track plays
 * from GET /api/v1/music/<track id>; the whole album is GET .../zip.
 * Each track is billed like one /music song.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return err(403, "cross-site request refused");
  const rl = rateLimit(`album:${user.id}`, 3);
  if (!rl.ok) return err(429, "rate limit: too many albums this hour", { "retry-after": String(rl.retryAfter) });
  const parsed = parseAlbumRequest(await req.json().catch(() => null));
  if (typeof parsed === "string") return err(400, parsed);

  let upstream: Response;
  try {
    upstream = await infernetFor(user.coinpay_sub)("/api/chat/album", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(parsed),
      // Free callers spend one free query per track before it answers.
      signal: AbortSignal.timeout(30_000),
    });
  } catch (e) {
    return err(502, `network unreachable: ${e instanceof Error ? e.message : e}`);
  }
  const body = (await upstream.json().catch(() => null)) as Record<string, unknown> | null;
  if (upstream.status !== 202 || typeof body?.id !== "string") {
    const message = typeof body?.error === "string" ? body.error : `album could not start (upstream HTTP ${upstream.status})`;
    return err(upstream.status >= 400 ? upstream.status : 502, message);
  }
  return json(
    { id: body.id, status: "pending", tracks: body.tracks ?? null, limited: body.limited === true, poll: `/api/v1/album/${body.id}` },
    202,
    { "cache-control": "no-store" },
  );
}
