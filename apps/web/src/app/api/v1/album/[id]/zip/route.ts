import { isJobId } from "@/lib/chat/images";
import { currentUser, headerTimeout, infernet, json, ownsJob, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/album/<id>/zip: the finished album, "Band - Album/" with
 * cover.jpg, album.txt, "NN - Title.mp3" (tagged with lyrics and cover) and
 * "NN - Title-lyrics.txt" per track.
 */
export async function GET(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { id } = await params;
  if (!isJobId(id)) return json({ error: { message: "unknown album", code: 404 } }, 404);
  // Only the account that started a job reads its result.
  if (!(await ownsJob(user.id, id))) return json({ error: { message: "unknown album", code: 404 } }, 404);
  let upstream: Response;
  // Headers only: the zip itself may take longer than any fixed limit to send.
  const wait = headerTimeout(300_000);
  try {
    upstream = await infernet(`/api/chat/album/${id}/zip`, { signal: wait.signal });
  } catch (e) {
    return json({ error: { message: `network unreachable: ${e instanceof Error ? e.message : e}`, code: 502 } }, 502);
  } finally {
    wait.clear();
  }
  if (upstream.status === 200 && (upstream.headers.get("content-type") ?? "").startsWith("application/zip")) {
    const headers: Record<string, string> = { "content-type": "application/zip", "cache-control": "private, no-store" };
    for (const h of ["content-length", "content-disposition"]) {
      const v = upstream.headers.get(h);
      if (v) headers[h] = v;
    }
    headers["content-disposition"] ??= 'attachment; filename="c0mpute-album.zip"';
    return new Response(upstream.body, { status: 200, headers });
  }
  const body = (await upstream.json().catch(() => ({}))) as Record<string, unknown>;
  return json({ error: { message: typeof body.error === "string" ? body.error : "no album download (yet)", code: upstream.status } }, upstream.status >= 400 ? upstream.status : 502);
}
