import { isJobId } from "@/lib/chat/images";
import { sanitizeAlbum } from "@/lib/chat/album";
import { currentUser, infernet, json, ownsJob, recordJob, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/album/<id>: the album's plan and every track's progress.
 * 202 while it is being made, 200 when done (with `zip` and `cover` links).
 */
export async function GET(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { id } = await params;
  if (!isJobId(id)) return json({ error: { message: "unknown album", code: 404 } }, 404);
  // Only the account that started a job reads its result.
  if (!(await ownsJob(user.id, id))) return json({ error: { message: "unknown album", code: 404 } }, 404);
  let upstream: Response;
  try {
    upstream = await infernet(`/api/chat/album/${id}`, { signal: AbortSignal.timeout(30_000) });
  } catch (e) {
    return json({ error: { message: `network unreachable: ${e instanceof Error ? e.message : e}`, code: 502 } }, 502);
  }
  if (!(upstream.headers.get("content-type") ?? "").includes("json")) {
    await upstream.body?.cancel().catch(() => {});
    return json({ error: { message: "the c0mpute network is restarting; try again in a minute", code: 503 } }, 503);
  }
  const body = (await upstream.json().catch(() => ({}))) as Record<string, unknown>;
  if (upstream.status !== 202 && upstream.status !== 200) {
    return json({ error: { message: typeof body.error === "string" ? body.error : "album failed", code: upstream.status } }, upstream.status >= 400 ? upstream.status : 502);
  }
  const album = sanitizeAlbum(body);
  // Each track is a song job of its own (played from /api/v1/music/<id>): it
  // belongs to whoever owns the album.
  await Promise.all(album.tracks.map((t) => (t.id ? recordJob(user.id, "music", t.id) : null)));
  const done = upstream.status === 200;
  return json(
    {
      id,
      status: done ? "done" : "pending",
      seconds: typeof body.seconds === "number" ? body.seconds : null,
      ...album,
      cover: album.cover ? `/api/v1/album/${id}/cover` : null,
      zip: done ? `/api/v1/album/${id}/zip` : null,
    },
    done ? 200 : 202,
    { "cache-control": "no-store" },
  );
}
