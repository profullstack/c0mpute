import { json, liveStore } from "@/lib/chat/server";
import { liveErr, liveUnauthorized, liveUser } from "@/lib/live/http";
import { LIVE_USD_PER_GB } from "@/lib/live/store";

export const dynamic = "force-dynamic";

/** GET /api/v1/live/streams/:id/usage (live:publish): what relays carried and what it cost. */
export async function GET(req: Request, ctx: { params: Promise<{ id: string }> }) {
  const user = await liveUser(req);
  if (!user) return liveUnauthorized();
  try {
    const store = liveStore();
    const s = await store.get(user.id, (await ctx.params).id);
    if (!s) return json({ error: { code: "not_found", message: "no such stream on this account" } }, 404);
    return json({ id: s.id, usd_per_gb: LIVE_USD_PER_GB, ...(await store.usage(s.id)) });
  } catch (e) {
    return liveErr(e);
  }
}
