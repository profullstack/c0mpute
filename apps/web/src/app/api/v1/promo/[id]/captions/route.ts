import { isJobId } from "@/lib/chat/images";
import { currentUser, infernet, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/promo/<id>/captions: the narration as SubRip (.srt). */
export async function GET(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { id } = await params;
  if (!isJobId(id)) return json({ error: { message: "unknown promo job", code: 404 } }, 404);
  let upstream: Response;
  try {
    upstream = await infernet(`/api/chat/promo/${id}/captions`, { signal: AbortSignal.timeout(20_000) });
  } catch (e) {
    return json({ error: { message: `network unreachable: ${e instanceof Error ? e.message : e}`, code: 502 } }, 502);
  }
  if (upstream.status === 200 && (upstream.headers.get("content-type") ?? "").startsWith("application/x-subrip")) {
    return new Response(await upstream.text(), {
      status: 200,
      headers: { "content-type": "application/x-subrip; charset=utf-8", "content-disposition": 'attachment; filename="c0mpute-promo.srt"', "cache-control": "private, max-age=3600" },
    });
  }
  await upstream.body?.cancel().catch(() => {});
  return json({ error: { message: "no captions (yet)", code: 404 } }, 404);
}
