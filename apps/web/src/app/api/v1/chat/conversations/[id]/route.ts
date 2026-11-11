import { parseConversation } from "@/lib/chat/convo-store";
import { convoStore, currentUser, json, sameOrigin, unauthorized } from "@/lib/chat/server";
import { convoErr } from "@/lib/chat/convo-http";

export const dynamic = "force-dynamic";

type Ctx = { params: Promise<{ id: string }> };
const forbidden = () => json({ error: { message: "cross-site request refused", code: 403 } }, 403);

/** GET /api/v1/chat/conversations/<id>: one conversation with its messages. */
export async function GET(req: Request, { params }: Ctx) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { id } = await params;
  try {
    const c = await convoStore().get(user.id, id);
    if (!c) return json({ error: { message: `no conversation ${id} on this account`, code: 404 } }, 404);
    return json(c);
  } catch (e) {
    return convoErr(e);
  }
}

/**
 * PUT /api/v1/chat/conversations/<id> { title, model, messages, updated }.
 * Last writer wins on `updated`; an older copy gets 409 with the newer one.
 */
export async function PUT(req: Request, { params }: Ctx) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return forbidden();
  const { id } = await params;
  try {
    const c = parseConversation(id, await req.json().catch(() => null));
    return json(await convoStore().put(user.id, c));
  } catch (e) {
    return convoErr(e);
  }
}

/** DELETE /api/v1/chat/conversations/<id>: gone on every device at its next sync. */
export async function DELETE(req: Request, { params }: Ctx) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return forbidden();
  const { id } = await params;
  try {
    const gone = await convoStore().remove(user.id, id);
    if (!gone) return json({ error: { message: `no conversation ${id} on this account`, code: 404 } }, 404);
    return json({ id, deleted: true });
  } catch (e) {
    return convoErr(e);
  }
}
