import { publicPush, SITE_NAME, validatePushFiles } from "@/lib/chat/push";
import { bad, pickNode, pushErr } from "@/lib/chat/push-http";
import { currentUser, json, pushStore, sameOrigin, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/hosting/pushes: this account's hosting pushes (newest first). */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  try {
    const ps = pushStore();
    const [pushes, nodes] = await Promise.all([ps.pushes(user.id, "hosting"), ps.nodes(user.id)]);
    const byId = new Map(nodes.map((n) => [n.id, n]));
    return json({ pushes: pushes.map((p) => publicPush(p, byId.get(p.node_id))) });
  } catch (e) {
    return pushErr(e);
  }
}

/**
 * POST /api/v1/hosting/pushes {name, node?, files}: `/push hosting <name>`.
 * Queues a publish on one of the account's nodes; pushing an existing name
 * ships a new version of that site. Answers 202 with the push to poll.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return bad("cross-site request refused", 403);
  const body = (await req.json().catch(() => null)) as Record<string, unknown> | null;
  if (!body) return bad("JSON body required");
  const name = String(body.name ?? "").toLowerCase();
  if (!SITE_NAME.test(name)) return bad("site name: 1-63 of a-z, 0-9 and '-'");
  const files = validatePushFiles(body.files);
  if (typeof files === "string") return bad(files);
  try {
    const ps = pushStore();
    const existing = await ps.push(user.id, "hosting", name);
    // An existing site stays on the node that signed it unless one is named.
    const node = await pickNode(ps, user.id, body.node ?? (existing?.site_id ? existing.node_id : undefined));
    if (node instanceof Response) return node;
    const push = await ps.upsertPush(user.id, { target: "hosting", name, node, files, disks: "auto" });
    return json({ push: publicPush(push, node), poll: `/api/v1/hosting/pushes/${push.id}` }, 202);
  } catch (e) {
    return pushErr(e);
  }
}
