import { json, liveStore, sameOrigin, siteUrl } from "@/lib/chat/server";
import { liveErr, liveRate, liveUnauthorized, liveUser, tierFor } from "@/lib/live/http";
import { LIVE_RTMP_URL, streamJson } from "@/lib/live/store";

export const dynamic = "force-dynamic";

/** GET /api/v1/live/streams (live:publish): the person's streams. Never their keys. */
export async function GET(req: Request) {
  const user = await liveUser(req);
  if (!user) return liveUnauthorized();
  try {
    const site = siteUrl();
    return json({ streams: (await liveStore().list(user.id)).map((s) => streamJson(s, site)) });
  } catch (e) {
    return liveErr(e);
  }
}

/**
 * POST /api/v1/live/streams (live:publish) {name, title?, visibility?}: a new
 * stream. `stream_key` is in this reply and never again (rotate for a new one).
 * Push RTMP to `publish_url`; anyone watches at `watch_url`.
 */
export async function POST(req: Request) {
  const user = await liveUser(req);
  if (!user) return liveUnauthorized();
  if (!sameOrigin(req)) return json({ error: { code: "forbidden", message: "cross-site request" } }, 403);
  if (!liveRate(user.id).ok) return json({ error: { code: "rate_limited", message: "slow down" } }, 429);
  try {
    const store = liveStore();
    const { stream, stream_key } = await store.create(user, await req.json().catch(() => null));
    const tier = await tierFor(store, user);
    return json({ ...streamJson(stream, siteUrl()), tier: tier.paid ? "paid" : "free", stream_key, publish_url: `${LIVE_RTMP_URL}/${stream_key}` }, 201);
  } catch (e) {
    return liveErr(e);
  }
}
