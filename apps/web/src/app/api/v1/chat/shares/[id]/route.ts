import { currentUser, json, sameOrigin, shareStore, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/chat/shares/<id>: a shared chat's messages (public, like the page), for "continue this chat". */
export async function GET(_req: Request, { params }: { params: Promise<{ id: string }> }) {
  const share = await shareStore()
    .get((await params).id)
    .catch(() => null);
  if (!share) return json({ error: { message: "this shared chat was removed or never existed", code: 404 } }, 404);
  return json(share, 200, { "x-robots-tag": "noindex" });
}

/** DELETE /api/v1/chat/shares/<id>: unshare (the link stops working). Owner only. */
export async function DELETE(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const ok = await shareStore()
    .revoke(user.id, (await params).id)
    .catch(() => false);
  return ok ? json({ ok: true }) : json({ error: { message: "no such shared chat", code: 404 } }, 404);
}
