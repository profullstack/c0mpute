import { isJobId } from "@/lib/chat/images";
import { currentUser, infernet, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/images/<id>: 202 while a worker draws, then image/png once. */
export async function GET(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { id } = await params;
  if (!isJobId(id)) return json({ error: { message: "unknown image job", code: 404 } }, 404);
  let upstream: Response;
  try {
    upstream = await infernet(`/api/chat/image/${id}`, { signal: AbortSignal.timeout(20_000) });
  } catch (e) {
    return json({ error: { message: `network unreachable: ${e instanceof Error ? e.message : e}`, code: 502 } }, 502);
  }
  const type = upstream.headers.get("content-type") ?? "";
  if (upstream.status === 200 && type.startsWith("image/")) {
    return new Response(upstream.body, {
      status: 200,
      headers: {
        "content-type": type,
        "cache-control": "no-store",
        ...(upstream.headers.get("x-c0mpute-worker-peer-id")
          ? { "x-c0mpute-worker-peer-id": upstream.headers.get("x-c0mpute-worker-peer-id")! }
          : {}),
      },
    });
  }
  if (!type.includes("json")) {
    await upstream.body?.cancel().catch(() => {});
    return json({ error: { message: "the c0mpute network is restarting; try again in a minute", code: 503 } }, 503);
  }
  const body = (await upstream.json().catch(() => ({}))) as Record<string, unknown>;
  if (upstream.status === 202) {
    // While queued for a free worker the network says so (stage "queued",
    // position, one-line detail); pass that through for the chat to show.
    const str = (v: unknown) => (typeof v === "string" ? v.slice(0, 200) : undefined);
    const num = (v: unknown) => (typeof v === "number" && Number.isFinite(v) ? v : undefined);
    return json(
      { id, status: "pending", seconds: body.seconds ?? null, stage: str(body.stage), detail: str(body.detail), queue_position: num(body.queue_position) },
      202,
      { "cache-control": "no-store" },
    );
  }
  return json({ error: { message: typeof body.error === "string" ? body.error : "image failed", code: upstream.status } }, upstream.status);
}
