import { pluginErr } from "@/lib/chat/plugins";
import { publicPlugin } from "@/lib/chat/plugin-store";
import { currentUser, json, pluginStore, sameOrigin, siteUrl, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/chat/plugins/<slug> */
export async function GET(req: Request, { params }: { params: Promise<{ slug: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { slug } = await params;
  try {
    const p = await pluginStore().get(user.id, slug);
    if (!p) return json({ error: { message: `no chat plugin ${slug} on this account`, code: 404 } }, 404);
    return json({ plugin: publicPlugin(p, siteUrl()) });
  } catch (e) {
    return pluginErr(e);
  }
}

/** DELETE /api/v1/chat/plugins/<slug>: remove it and forget its tokens. */
export async function DELETE(req: Request, { params }: { params: Promise<{ slug: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const { slug } = await params;
  try {
    const gone = await pluginStore().remove(user.id, slug);
    if (!gone) return json({ error: { message: `no chat plugin ${slug} on this account`, code: 404 } }, 404);
    return json({ ok: true, removed: slug });
  } catch (e) {
    return pluginErr(e);
  }
}
