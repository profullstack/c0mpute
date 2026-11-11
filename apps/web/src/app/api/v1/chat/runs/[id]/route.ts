import { SSE_HEADERS, cancelRun, getRun, runStream } from "@/lib/chat/runs";
import { currentUser, json, sameOrigin, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";
export const maxDuration = 300;

type Ctx = { params: Promise<{ id: string }> };
const gone = (id: string) =>
  json({ error: { message: `no run ${id}: it finished more than 15 minutes ago, or the server restarted`, code: 404 } }, 404);

/** GET /api/v1/chat/runs/<id>[?from=<bytes>]: replay the turn's SSE, then follow it live. */
export async function GET(req: Request, { params }: Ctx) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { id } = await params;
  const run = getRun(user.id, id);
  if (!run) return gone(id);
  const from = Number(new URL(req.url).searchParams.get("from") ?? 0);
  return new Response(runStream(run, Number.isFinite(from) ? from : 0), {
    headers: { ...SSE_HEADERS, "x-c0mpute-run": run.id, "x-c0mpute-run-status": run.status },
  });
}

/** DELETE /api/v1/chat/runs/<id>: stop a resumable turn (closing its connection does not). */
export async function DELETE(req: Request, { params }: Ctx) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const { id } = await params;
  if (!cancelRun(user.id, id)) return gone(id);
  return json({ id, cancelled: true });
}
