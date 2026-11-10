import { isVideoJobId } from "@/lib/chat/videos";
import { currentUser, infernet, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/videos/<id>/video: the MP4, with Range support (206) for players and resumed downloads. */
export async function GET(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { id } = await params;
  if (!isVideoJobId(id)) return json({ error: { message: "unknown video job", code: 404 } }, 404);
  const range = req.headers.get("range");
  let upstream: Response;
  try {
    upstream = await infernet(`/api/chat/video/${id}/video`, {
      headers: range ? { range } : {},
      signal: AbortSignal.timeout(120_000),
    });
  } catch (e) {
    return json({ error: { message: `network unreachable: ${e instanceof Error ? e.message : e}`, code: 502 } }, 502);
  }
  if (upstream.status === 200 || upstream.status === 206 || upstream.status === 416) {
    const headers: Record<string, string> = { "content-type": "video/mp4", "accept-ranges": "bytes", "cache-control": "private, no-store" };
    for (const h of ["content-length", "content-range", "x-c0mpute-worker-peer-id"]) {
      const v = upstream.headers.get(h);
      if (v) headers[h] = v;
    }
    return new Response(upstream.status === 416 ? null : upstream.body, { status: upstream.status, headers });
  }
  const body = (await upstream.json().catch(() => ({}))) as Record<string, unknown>;
  return json({ error: { message: typeof body.error === "string" ? body.error : "video not available", code: upstream.status } }, upstream.status);
}
