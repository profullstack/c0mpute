import { isJobId } from "@/lib/chat/images";
import { currentUser, infernet, json, ownsJob, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/promo/<id>/frame/<n>: storyboard keyframe n (PNG) once it is drawn. */
export async function GET(req: Request, { params }: { params: Promise<{ id: string; n: string }> }) {
  const user = await currentUser(req, { oauth: true });
  if (!user) return unauthorized();
  const { id, n } = await params;
  const i = Number(n);
  if (!isJobId(id) || !Number.isInteger(i) || i < 0 || i > 15) return json({ error: { message: "unknown frame", code: 404 } }, 404);
  // Only the account that started a job reads its result.
  if (!(await ownsJob(user.id, id))) return json({ error: { message: "unknown frame", code: 404 } }, 404);
  let upstream: Response;
  try {
    upstream = await infernet(`/api/chat/promo/${id}/frame/${i}`, { signal: AbortSignal.timeout(20_000) });
  } catch (e) {
    return json({ error: { message: `network unreachable: ${e instanceof Error ? e.message : e}`, code: 502 } }, 502);
  }
  if (upstream.status === 200 && (upstream.headers.get("content-type") ?? "").startsWith("image/png")) {
    return new Response(upstream.body, { status: 200, headers: { "content-type": "image/png", "cache-control": "private, max-age=3600" } });
  }
  await upstream.body?.cancel().catch(() => {});
  return json({ error: { message: "not drawn yet", code: 404 } }, 404);
}
