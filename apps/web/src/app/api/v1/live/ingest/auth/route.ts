import { json, liveStore } from "@/lib/chat/server";
import { ingestAuthorized, liveErr, tierFor } from "@/lib/live/http";

export const dynamic = "force-dynamic";

/**
 * POST /api/v1/live/ingest/auth (ingest nodes only) {stream_key}: whether a
 * key may publish, and on which tier. Asked before any media is accepted.
 */
export async function POST(req: Request) {
  if (!ingestAuthorized(req)) return json({ ok: false }, 401);
  try {
    const store = liveStore();
    const body = (await req.json().catch(() => null)) as { stream_key?: unknown } | null;
    const s = await store.byKey(body?.stream_key);
    if (!s) return json({ ok: false }, 403);
    const tier = await tierFor(store, { id: s.user_id, coinpay_sub: s.coinpay_sub });
    await store.setPaid(s.id, tier.paid);
    return json({ ok: true, id: s.id, account: s.user_id, name: s.name, ...tier });
  } catch (e) {
    return liveErr(e);
  }
}
