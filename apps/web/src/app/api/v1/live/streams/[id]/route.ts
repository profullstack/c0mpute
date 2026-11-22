import { json, liveStore, sameOrigin, siteUrl } from "@/lib/chat/server";
import { liveErr, liveUnauthorized, liveUser } from "@/lib/live/http";
import { streamJson } from "@/lib/live/store";

export const dynamic = "force-dynamic";

const notFound = () => json({ error: { code: "not_found", message: "no such stream on this account" } }, 404);

/** GET /api/v1/live/streams/:id (live:publish). */
export async function GET(req: Request, ctx: { params: Promise<{ id: string }> }) {
  const user = await liveUser(req);
  if (!user) return liveUnauthorized();
  try {
    const s = await liveStore().get(user.id, (await ctx.params).id);
    return s ? json(streamJson(s, siteUrl())) : notFound();
  } catch (e) {
    return liveErr(e);
  }
}

/** DELETE /api/v1/live/streams/:id (live:publish): the key stops working at once. */
export async function DELETE(req: Request, ctx: { params: Promise<{ id: string }> }) {
  const user = await liveUser(req);
  if (!user) return liveUnauthorized();
  if (!sameOrigin(req)) return json({ error: { code: "forbidden", message: "cross-site request" } }, 403);
  try {
    return (await liveStore().remove(user.id, (await ctx.params).id)) ? json({ ok: true }) : notFound();
  } catch (e) {
    return liveErr(e);
  }
}
