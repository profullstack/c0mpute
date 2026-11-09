import { isJobId } from "@/lib/chat/images";
import { currentUser, infernet, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/promo/<id>/video: the MP4 (byte ranges pass through; ?download=1 saves it). */
export async function GET(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { id } = await params;
  if (!isJobId(id)) return json({ error: { message: "unknown promo job", code: 404 } }, 404);
  const download = new URL(req.url).searchParams.get("download") === "1";
  const range = req.headers.get("range");
  let upstream: Response;
  try {
    upstream = await infernet(`/api/chat/promo/${id}/video`, { headers: range ? { range } : {}, signal: AbortSignal.timeout(120_000) });
  } catch (e) {
    return json({ error: { message: `network unreachable: ${e instanceof Error ? e.message : e}`, code: 502 } }, 502);
  }
  if ((upstream.status === 200 || upstream.status === 206) && (upstream.headers.get("content-type") ?? "").startsWith("video/mp4")) {
    const headers: Record<string, string> = { "content-type": "video/mp4", "accept-ranges": "bytes", "cache-control": "private, max-age=3600" };
    for (const h of ["content-length", "content-range"]) {
      const v = upstream.headers.get(h);
      if (v) headers[h] = v;
    }
    if (download) headers["content-disposition"] = 'attachment; filename="c0mpute-promo.mp4"';
    return new Response(upstream.body, { status: upstream.status, headers });
  }
  if (upstream.status === 416) return new Response(null, { status: 416, headers: { "content-range": upstream.headers.get("content-range") ?? "" } });
  const body = (await upstream.json().catch(() => ({}))) as Record<string, unknown>;
  return json({ error: { message: typeof body.error === "string" ? body.error : "no video (yet)", code: upstream.status } }, upstream.status >= 400 ? upstream.status : 502);
}
