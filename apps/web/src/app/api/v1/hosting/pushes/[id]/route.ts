import { publicPush, SITE_NAME } from "@/lib/chat/push";
import { bad, pushErr } from "@/lib/chat/push-http";
import { currentUser, json, pushStore, sameOrigin, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** A push by id (p_...) or by site name, only ever within this account. */
async function find(userId: string, ref: string) {
  const ps = pushStore();
  return ref.startsWith("p_") ? ps.pushById(userId, ref) : SITE_NAME.test(ref) ? ps.push(userId, "hosting", ref) : null;
}

/** GET /api/v1/hosting/pushes/<id|name>: status, the node's live progress line, url once live. */
export async function GET(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const { id } = await params;
  try {
    const p = await find(user.id, id);
    if (!p) return bad("no such push on this account", 404);
    const node = await pushStore().node(user.id, p.node_id);
    return json({ push: publicPush(p, node) });
  } catch (e) {
    return pushErr(e);
  }
}

/**
 * DELETE /api/v1/hosting/pushes/<id|name>: `/push hosting rm <name>`. A live
 * site becomes a delete job for its node (`hosting rm`); 202 until it is gone.
 */
export async function DELETE(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return bad("cross-site request refused", 403);
  const { id } = await params;
  try {
    const p = await find(user.id, id);
    if (!p) return bad("no such push on this account", 404);
    const r = await pushStore().deletePush(user.id, p.target, p.name);
    return r === "gone" ? json({ ok: true, status: "deleted" }) : json({ ok: true, status: "deleting", poll: `/api/v1/hosting/pushes/${p.id}` }, 202);
  } catch (e) {
    return pushErr(e);
  }
}

/**
 * POST /api/v1/hosting/pushes/<id> {node, rev, progress} or {node, rev, ok,
 * site_id, version, url, copies, bytes, error}: the node reports on a job.
 */
export async function POST(req: Request, { params }: { params: Promise<{ id: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (user.token_kind !== "key") return bad("only a linked node reports on pushes", 403);
  const { id } = await params;
  const body = (await req.json().catch(() => null)) as Record<string, unknown> | null;
  if (!body || typeof body.node !== "string") return bad("node and rev required");
  try {
    const r = await pushStore().report(user.id, body.node, id, body);
    return r === "deleted" ? json({ ok: true, status: "deleted" }) : json({ ok: true, push: publicPush(r) });
  } catch (e) {
    return pushErr(e);
  }
}
