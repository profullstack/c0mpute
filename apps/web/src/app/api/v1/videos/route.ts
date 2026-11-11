import { gateGeneration } from "@/lib/chat/adult";
import { isVideoJobId, parseVideoRequest, preferWait } from "@/lib/chat/videos";
import { balanceFor, currentUser, env, infernet, infernetFor, json, rateLimit, sameOrigin, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";
// A held request (Prefer: wait=N) can stay open up to 2 h.
export const maxDuration = 7200;

const err = (status: number, message: string, headers: Record<string, string> = {}) =>
  json({ error: { message, type: "c0mpute_error", code: status } }, status, headers);

/** How long this deployment's proxy holds a request open; past it we answer 202. */
const holdCap = () => {
  const v = Number(env("C0MPUTE_VIDEO_MAX_HOLD_S"));
  return Number.isFinite(v) && v >= 0 && env("C0MPUTE_VIDEO_MAX_HOLD_S") !== "" ? v : 7200;
};

/**
 * Generate a video clip on the c0mpute network (Wan 2.1 1.3B on CPU workers
 * by default; 20-30 minutes once a worker is free, queued up to 2 h).
 *
 *   POST { prompt, model?, negative_prompt?, seed?, adult?: true }
 *     -> 202 { id, status, poll, video }       (default)
 *     -> 200 video/mp4                          (with `Prefer: wait=N` and done within N s, N <= 7200)
 *
 * Poll GET /api/v1/videos/<id>; fetch GET /api/v1/videos/<id>/video (Range
 * supported). Auth: the chat session or `Authorization: Bearer c0k_...`.
 * Billed to the person's compute balance per clip, or one free daily query.
 * Adult mode needs an 18+ attestation and a paid balance, and is text only.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return err(403, "cross-site request refused");
  const rl = rateLimit(`video:${user.id}`, 6);
  if (!rl.ok) return err(429, "rate limit: too many videos this hour", { "retry-after": String(rl.retryAfter) });
  const parsed = parseVideoRequest(await req.json().catch(() => null));
  if (typeof parsed === "string") return err(400, parsed);
  const gate = gateGeneration(
    user,
    { prompt: parsed.prompt, negative: parsed.negative_prompt, adult: parsed.adult === true },
    parsed.adult ? await balanceFor(user.coinpay_sub) : null,
  );
  if (!gate.ok) return json({ error: { message: gate.message, type: "content_policy", code: gate.code } }, gate.status);

  let upstream: Response;
  try {
    upstream = await infernetFor(user.coinpay_sub)("/api/chat/video", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(parsed),
      signal: AbortSignal.timeout(20_000),
    });
  } catch (e) {
    return err(502, `network unreachable: ${e instanceof Error ? e.message : e}`);
  }
  const body = (await upstream.json().catch(() => null)) as Record<string, unknown> | null;
  if (upstream.status !== 202 || typeof body?.id !== "string" || !isVideoJobId(body.id)) {
    const message = typeof body?.error === "string" ? body.error : `video could not start (upstream HTTP ${upstream.status})`;
    return err(upstream.status >= 400 ? upstream.status : 502, message);
  }
  const id = body.id;
  const accepted = (extra: Record<string, unknown> = {}) =>
    json({ id, status: "pending", poll: `/api/v1/videos/${id}`, video: `/api/v1/videos/${id}/video`, ...extra }, 202, {
      "cache-control": "no-store",
      location: `/api/v1/videos/${id}`,
    });

  const wait = preferWait(req.headers.get("prefer"), holdCap());
  if (wait <= 0) return accepted();
  const until = Date.now() + wait * 1000;
  while (Date.now() < until) {
    await new Promise((r) => setTimeout(r, Math.min(5000, Math.max(0, until - Date.now()))));
    if (req.signal.aborted) return accepted();
    let poll: Response;
    try {
      poll = await infernet(`/api/chat/video/${id}`, { signal: AbortSignal.timeout(20_000) });
    } catch {
      continue;
    }
    const p = (await poll.json().catch(() => null)) as Record<string, unknown> | null;
    if (poll.status === 202) continue;
    // A deploy of the network's web tier answers 502/503 (or an HTML
    // maintenance page) for a minute: keep holding, the job is not lost there.
    if (!p || poll.status >= 500) continue;
    if (poll.status !== 200) {
      return err(poll.status >= 400 ? poll.status : 502, typeof p.error === "string" ? p.error : "video failed");
    }
    const mp4 = await infernet(`/api/chat/video/${id}/video`, { signal: AbortSignal.timeout(120_000) }).catch(() => null);
    if (!mp4 || mp4.status !== 200) break;
    return new Response(mp4.body, {
      status: 200,
      headers: {
        "content-type": "video/mp4",
        "cache-control": "private, no-store",
        "x-video-id": id,
        ...(mp4.headers.get("content-length") ? { "content-length": mp4.headers.get("content-length")! } : {}),
        ...(mp4.headers.get("x-c0mpute-worker-peer-id") ? { "x-c0mpute-worker-peer-id": mp4.headers.get("x-c0mpute-worker-peer-id")! } : {}),
      },
    });
  }
  return accepted({ note: "not finished within the wait; poll the status URL" });
}
