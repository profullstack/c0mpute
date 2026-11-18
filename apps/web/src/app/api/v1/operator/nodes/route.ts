import { bad } from "@/lib/chat/push-http";
import { currentUser, json, sameOrigin, unauthorized } from "@/lib/chat/server";
import { parseCost } from "@/lib/earnings/report";
import { earningsStore, operatorNodes } from "@/lib/earnings/server";
import { nodeKey } from "@/lib/earnings/store";
import { getNodesPayload } from "@/lib/nodes";

export const dynamic = "force-dynamic";

const label = (v: unknown) => (typeof v === "string" ? v.trim().slice(0, 64) || null : undefined);

/** GET /api/v1/operator/nodes: the nodes this account counts as its own. */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  return json({ nodes: await operatorNodes(user.id) });
}

/**
 * POST /api/v1/operator/nodes {node: "<peer id or 8-char board id>", label?}
 * or {nickname: "<c0mpute name>"} to add every node online under that name.
 *
 * Run from the node itself with its chat key and the claim is marked as made
 * from the node:
 *   curl -sS https://c0mpute.com/api/v1/operator/nodes \
 *     -H "authorization: Bearer $C0MPUTE_API_KEY" \
 *     -d "{\"node\":\"$(c0mpute status --json | jq -r .peer_id)\"}"
 *
 * Claiming shows a node's earnings on your dashboard, nothing more: the job
 * history is public on /nodes already, and no money moves.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return bad("cross-site request refused", 403);
  const body = (await req.json().catch(() => ({}))) as Record<string, unknown>;
  const via = user.token_kind === "key" ? "key" : "web";
  const store = earningsStore();
  const board = await getNodesPayload().catch(() => null);
  const rows = board?.nodes ?? [];
  try {
    if (typeof body.nickname === "string" && body.nickname.trim()) {
      const name = body.nickname.trim().toLowerCase();
      const matches = rows.filter((n) => (n.name ?? "").toLowerCase() === name);
      if (matches.length === 0) return bad(`no node online is named "${name}"`, 404);
      const added = [];
      for (const n of matches) added.push(await store.claim(user.id, { node: n.id, class: n.class, via }));
      return json({ nodes: added }, 201);
    }
    const k = nodeKey(body.node);
    if (!k) return bad("node: a peer id (c0mpute status --json | jq -r .peer_id) or the 8-character id on /nodes");
    const row = rows.find((n) => n.id === k.node);
    const node = await store.claim(user.id, { node: k.node, peer_id: k.peer_id, label: label(body.label) ?? null, class: row?.class ?? null, via });
    return json({ node, online: !!row }, 201);
  } catch (e) {
    return bad(e instanceof Error ? e.message : "could not add the node", 409);
  }
}

/**
 * PATCH /api/v1/operator/nodes {node, cost, per: "hour"|"month", label?}
 * What the node costs to run (rent, or power you measured). cost null or ""
 * goes back to the power estimate.
 */
export async function PATCH(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return bad("cross-site request refused", 403);
  const body = (await req.json().catch(() => ({}))) as Record<string, unknown>;
  const k = nodeKey(body.node);
  if (!k) return bad("node: which node to change");
  const patch: { cost_usd_per_hour?: number | null; label?: string | null } = {};
  if ("cost" in body) {
    const c = parseCost(body.cost, body.per);
    if (c === "invalid") return bad("cost: a dollar amount from 0 to 1000 an hour, per hour or per month");
    patch.cost_usd_per_hour = c;
  }
  const l = label(body.label);
  if (l !== undefined) patch.label = l;
  const store = earningsStore();
  let ok = await store.update(user.id, k.node, patch);
  if (!ok) {
    // A hosting-linked node has no claim row yet: make one, then set it.
    const mine = (await operatorNodes(user.id)).find((n) => n.node === k.node);
    if (!mine) return bad("not one of your nodes", 404);
    await store.claim(user.id, { node: mine.node, peer_id: mine.peer_id, label: mine.label, via: mine.via });
    ok = await store.update(user.id, k.node, patch);
  }
  return ok ? json({ ok: true }) : bad("not one of your nodes", 404);
}

/** DELETE /api/v1/operator/nodes?node=<id>: stop counting a node as yours. */
export async function DELETE(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return bad("cross-site request refused", 403);
  const k = nodeKey(new URL(req.url).searchParams.get("node"));
  if (!k) return bad("node: which node to remove");
  const gone = await earningsStore().unclaim(user.id, k.node);
  return gone ? json({ ok: true }) : bad("not a node you added (hosting-linked nodes are removed with /connect hosting)", 404);
}
