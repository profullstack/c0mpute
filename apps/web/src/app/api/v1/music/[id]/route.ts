import { isJobId } from "@/lib/chat/images";
import { AUDIO_TYPES } from "@/lib/chat/music";
import { currentUser, infernet, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/music/<id>: 202 while a worker composes, then the audio once. */
export async function GET(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { id } = await params;
  if (!isJobId(id)) return json({ error: { message: "unknown music job", code: 404 } }, 404);
  let upstream: Response;
  try {
    upstream = await infernet(`/api/chat/music/${id}`, { signal: AbortSignal.timeout(30_000) });
  } catch (e) {
    return json({ error: { message: `network unreachable: ${e instanceof Error ? e.message : e}`, code: 502 } }, 502);
  }
  const type = (upstream.headers.get("content-type") ?? "").split(";")[0].trim().toLowerCase();
  if (upstream.status === 200 && AUDIO_TYPES.includes(type)) {
    const worker = upstream.headers.get("x-c0mpute-worker-peer-id");
    return new Response(upstream.body, {
      status: 200,
      headers: { "content-type": type, "cache-control": "no-store", ...(worker ? { "x-c0mpute-worker-peer-id": worker } : {}) },
    });
  }
  if (!type.includes("json")) {
    await upstream.body?.cancel().catch(() => {});
    return json({ error: { message: "the c0mpute network is restarting; try again in a minute", code: 503 } }, 503);
  }
  const body = (await upstream.json().catch(() => ({}))) as Record<string, unknown>;
  if (upstream.status === 202) {
    // Infernet writes lyrics from a brief first; pass what it wrote along.
    const pick = (k: string) => (typeof body[k] === "string" || typeof body[k] === "number" ? body[k] : undefined);
    return json(
      { id, status: "pending", seconds: body.seconds ?? null, stage: pick("stage"), caption: pick("caption"), lyrics: pick("lyrics"), duration: pick("duration") },
      202,
      { "cache-control": "no-store" },
    );
  }
  return json({ error: { message: typeof body.error === "string" ? body.error : "song failed", code: upstream.status } }, upstream.status);
}
