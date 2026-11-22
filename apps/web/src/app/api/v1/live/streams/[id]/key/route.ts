import { json, liveStore, sameOrigin } from "@/lib/chat/server";
import { liveErr, liveRate, liveUnauthorized, liveUser } from "@/lib/live/http";
import { LIVE_RTMP_URL } from "@/lib/live/store";

export const dynamic = "force-dynamic";

/** POST /api/v1/live/streams/:id/key (live:publish): a new stream key; the old one stops working at once. */
export async function POST(req: Request, ctx: { params: Promise<{ id: string }> }) {
  const user = await liveUser(req);
  if (!user) return liveUnauthorized();
  if (!sameOrigin(req)) return json({ error: { code: "forbidden", message: "cross-site request" } }, 403);
  if (!liveRate(user.id).ok) return json({ error: { code: "rate_limited", message: "slow down" } }, 429);
  try {
    const key = await liveStore().rotateKey(user.id, (await ctx.params).id);
    if (!key) return json({ error: { code: "not_found", message: "no such stream on this account" } }, 404);
    return json({ stream_key: key, publish_url: `${LIVE_RTMP_URL}/${key}` });
  } catch (e) {
    return liveErr(e);
  }
}
