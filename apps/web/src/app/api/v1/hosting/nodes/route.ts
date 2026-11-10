import { publicNode } from "@/lib/chat/push";
import { bad, pushErr } from "@/lib/chat/push-http";
import { currentUser, json, pushStore, sameOrigin, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/hosting/nodes: the hosting nodes linked to this account. */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  try {
    const nodes = await pushStore().nodes(user.id);
    return json({ nodes: nodes.map((n) => publicNode(n)) });
  } catch (e) {
    return pushErr(e);
  }
}

/**
 * POST /api/v1/hosting/nodes {name, peer_id, payments, version}: a node links
 * itself (`c0mpute chat connect hosting --name <node>`). API keys only: a
 * browser session cannot register a node.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (user.token_kind !== "key") return bad("link a node from the node itself: c0mpute chat login && c0mpute chat connect hosting --name <node>", 403);
  const body = (await req.json().catch(() => ({}))) as Record<string, unknown>;
  try {
    const node = await pushStore().registerNode(user.id, {
      name: String(body.name ?? ""),
      peer_id: body.peer_id,
      payments: body.payments,
      version: body.version,
    });
    return json({ node: publicNode(node) }, 201);
  } catch (e) {
    return pushErr(e);
  }
}

/** DELETE /api/v1/hosting/nodes?node=<name|id>: unlink a node (its sites stay up). */
export async function DELETE(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return bad("cross-site request refused", 403);
  const node = new URL(req.url).searchParams.get("node") ?? "";
  try {
    const gone = await pushStore().removeNode(user.id, node);
    return gone ? json({ ok: true }) : bad("no such node", 404);
  } catch (e) {
    return pushErr(e);
  }
}
