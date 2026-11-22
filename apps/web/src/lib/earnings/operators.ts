// Whose nodes are whose. No "server-only" import: the fleet alerts
// scheduler (instrumentation.ts) reads it too.
import { pushStore } from "../chat/server";
import { earningsStore } from "./recorder";
import { nodeKey, type OperatorNode } from "./store";

// ── the operator's nodes ─────────────────────────────────────────────────────

/**
 * This account's nodes: the ones it claimed, plus any linked for hosting
 * (`c0mpute chat connect hosting`), which already name their peer id.
 */
export async function operatorNodes(userId: string): Promise<OperatorNode[]> {
  const store = earningsStore();
  const claims = await store.claims(userId);
  const have = new Set(claims.map((c) => c.node));
  let linked: { name: string; peer_id: string | null; created_at: string }[] = [];
  try {
    linked = await pushStore().nodes(userId);
  } catch {
    // Hosting links are a bonus; the dashboard works without them.
  }
  for (const h of linked) {
    const k = nodeKey(h.peer_id);
    if (!k || have.has(k.node)) continue;
    have.add(k.node);
    claims.push({ node: k.node, peer_id: k.peer_id, label: h.name, class: null, cost_usd_per_hour: null, via: "hosting", created_at: h.created_at });
  }
  return claims;
}
