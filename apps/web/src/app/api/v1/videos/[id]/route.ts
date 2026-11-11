import { isVideoJobId } from "@/lib/chat/videos";
import { currentUser, infernet, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/videos/<id>: 202 while queued or rendering, 200 { status: "done", video } when ready. */
export async function GET(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { id } = await params;
  if (!isVideoJobId(id)) return json({ error: { message: "unknown video job", code: 404 } }, 404);
  let upstream: Response;
  try {
    upstream = await infernet(`/api/chat/video/${id}`, { signal: AbortSignal.timeout(20_000) });
  } catch (e) {
    return json({ error: { message: `network unreachable: ${e instanceof Error ? e.message : e}`, code: 502 } }, 502);
  }
  const body = (await upstream.json().catch(() => null)) as Record<string, unknown> | null;
  if (!body) return json({ error: { message: "the c0mpute network is restarting; try again in a minute", code: 503 } }, 503);
  const str = (v: unknown) => (typeof v === "string" ? v.slice(0, 200) : undefined);
  const num = (v: unknown) => (typeof v === "number" && Number.isFinite(v) ? v : undefined);
  if (upstream.status === 202) {
    return json(
      { id, status: "pending", seconds: num(body.seconds) ?? null, stage: str(body.stage), detail: str(body.detail), queue_position: num(body.queue_position), eta_s: num(body.eta_s) },
      202,
    );
  }
  if (upstream.status === 200) {
    return json({ id, status: "done", seconds: num(body.seconds) ?? null, bytes: num(body.bytes) ?? null, model: str(body.model) ?? null, adult: body.adult === true, video: `/api/v1/videos/${id}/video` });
  }
  return json({ error: { message: str(body.error) ?? "video failed", code: upstream.status } }, upstream.status);
}
