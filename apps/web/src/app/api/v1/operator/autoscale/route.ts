import { createAutoscaleStore, parseReport, type AutoscaleStore } from "@/lib/autoscale";
import { bad } from "@/lib/chat/push-http";
import { cloudSql, currentUser, json, sameOrigin, unauthorized } from "@/lib/chat/server";
import { earningsStore, operatorNodes } from "@/lib/earnings/server";

export const dynamic = "force-dynamic";

let _store: AutoscaleStore | null = null;
const autoscaleStore = () => (_store ??= createAutoscaleStore(cloudSql));

/** GET /api/v1/operator/autoscale: the autoscaled nodes on this account and their recent transitions. */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  // Only nodes still on the account: removing a node hides its autoscale row too.
  const [rows, mine] = await Promise.all([autoscaleStore().nodes(user.id), operatorNodes(user.id)]);
  const on = new Set(mine.map((n) => n.node));
  return json({ nodes: rows.filter((r) => on.has(r.node)) });
}

/**
 * POST /api/v1/operator/autoscale: `c0mpute autoscale` reports a node's state
 * (running, stopped, starting, stopping, replacing, error), why, and what
 * stopping it has saved so far. Sent with the operator's C0MPUTE_API_KEY.
 * The cloud provider's API key never comes here: the CLI keeps it.
 * A node not on the account yet is added to it.
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return bad("cross-site request refused", 403);
  const report = parseReport(await req.json().catch(() => null));
  if (typeof report === "string") return bad(report);
  try {
    const mine = await operatorNodes(user.id);
    if (!mine.some((n) => n.node === report.node)) {
      await earningsStore().claim(user.id, { node: report.node, peer_id: report.peer_id, via: user.token_kind === "key" ? "key" : "web" });
    }
    const { changed } = await autoscaleStore().report(user.id, report);
    return json({ ok: true, changed });
  } catch (e) {
    return bad(e instanceof Error ? e.message : "could not record the report", 409);
  }
}
