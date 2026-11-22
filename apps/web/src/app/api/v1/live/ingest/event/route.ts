import { json, liveStore } from "@/lib/chat/server";
import { ingestAuthorized, liveErr } from "@/lib/live/http";

export const dynamic = "force-dynamic";

/** POST /api/v1/live/ingest/event (ingest nodes only) {id, event: "start"|"end", stream_id, peer}. */
export async function POST(req: Request) {
  if (!ingestAuthorized(req)) return json({ ok: false }, 401);
  try {
    const ok = await liveStore().ingestEvent(await req.json().catch(() => null));
    return json({ ok }, ok ? 200 : 404);
  } catch (e) {
    return liveErr(e);
  }
}
