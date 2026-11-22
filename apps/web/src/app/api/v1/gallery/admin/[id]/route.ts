import { currentUser, sameOrigin } from "@/lib/chat/server";
import { gerror, gjson, readBody } from "@/lib/gallery/http";
import { gallery, isGalleryAdmin } from "@/lib/gallery/server";
import { ADMIN_ACTIONS, adminView, type AdminAction } from "@/lib/gallery/service";

export const dynamic = "force-dynamic";
export const maxDuration = 60;

/**
 * POST /api/v1/gallery/admin/<id> { action: approve|reject|feature|unfeature|requeue|delete, reason?, refund? }
 * Moderators only (session cookie from our own pages, or a c0k_ key).
 */
export async function POST(req: Request, ctx: { params: Promise<{ id: string }> }) {
  if (!sameOrigin(req)) return gjson({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const user = await currentUser(req);
  if (!(await isGalleryAdmin(user))) return gjson({ error: { message: "moderators only", code: 403 } }, user ? 403 : 401);
  const { id } = await ctx.params;
  const body = (await readBody(req)) ?? {};
  const action = body.action as AdminAction;
  if (!ADMIN_ACTIONS.includes(action)) return gjson({ error: { message: `action: ${ADMIN_ACTIONS.join(", ")}`, code: 400 } }, 400);
  try {
    const l = (await gallery().store.byId(id)) ?? (await gallery().store.bySlug(id));
    if (!l) return gjson({ error: { message: "no such listing", code: 404 } }, 404);
    const done = await gallery().admin(action, l, { reason: typeof body.reason === "string" ? body.reason : undefined, refund: body.refund === true || body.refund === "on" });
    return gjson({ action, listing: done ? adminView(done) : null });
  } catch (e) {
    return gerror(e);
  }
}
