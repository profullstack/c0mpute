import { isJobId } from "@/lib/chat/images";
import { sanitizePromoProgress } from "@/lib/chat/promo";
import { currentUser, infernet, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/promo/<id>: 202 { stages, scenes, ... } while it works, 200 with
 * { video, captions, duration } when done, or the error.
 */
export async function GET(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { id } = await params;
  if (!isJobId(id)) return json({ error: { message: "unknown promo job", code: 404 } }, 404);
  let upstream: Response;
  try {
    upstream = await infernet(`/api/chat/promo/${id}`, { signal: AbortSignal.timeout(20_000) });
  } catch (e) {
    return json({ error: { message: `network unreachable: ${e instanceof Error ? e.message : e}`, code: 502 } }, 502);
  }
  if (!(upstream.headers.get("content-type") ?? "").includes("json")) {
    await upstream.body?.cancel().catch(() => {});
    return json({ error: { message: "the c0mpute network is restarting; try again in a minute", code: 503 } }, 503);
  }
  const body = (await upstream.json().catch(() => ({}))) as Record<string, unknown>;
  if (upstream.status === 404 && typeof body.error === "string") return json({ error: { message: body.error, code: 404 } }, 404);
  const p = sanitizePromoProgress(id, body);
  if (p.status === "failed" || upstream.status >= 400) {
    return json({ ...p, error: { message: p.error ?? (typeof body.error === "string" ? body.error : "promo failed"), code: upstream.status } }, upstream.status >= 400 ? upstream.status : 502);
  }
  const base = `/api/v1/promo/${id}`;
  const done = p.status === "done";
  return json(
    { ...p, ...(done ? { video: `${base}/video`, captions: `${base}/captions` } : {}) },
    done ? 200 : 202,
    { "cache-control": "no-store" },
  );
}
