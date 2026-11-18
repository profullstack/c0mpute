// Server wiring for the operator dashboard: whose nodes are whose, Infernet
// provider credits, and the report. The recorder lives in ./recorder.
import "server-only";
import { infernet, pushStore } from "../chat/server";
import { getNodesPayload, type NodeRow } from "../nodes";
import { buildReport, RANGES, type InfernetHour, type RangeKey, type Report, type ReportNode } from "./report";
import { nodeKey, type OperatorNode } from "./store";
import { earningsStore } from "./recorder";

export { earningsStore };

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

// ── Infernet ─────────────────────────────────────────────────────────────────

/**
 * This person's Infernet provider credits per hour since `since`, read through
 * our trusted client key by their CoinPay subject. Null when Infernet is
 * unreachable or does not serve the endpoint yet.
 */
export async function infernetHours(subject: string, since: number): Promise<{ linked: boolean; hours: InfernetHour[]; providers: number } | null> {
  try {
    const r = await infernet(
      `/api/v1/billing/node-earnings?since=${encodeURIComponent(new Date(since).toISOString())}`,
      { signal: AbortSignal.timeout(8_000), cache: "no-store" },
      { subject },
    );
    if (!r.ok) return null;
    const raw = (await r.json()) as { data?: unknown };
    const d = (raw && typeof raw === "object" && "data" in raw ? raw.data : raw) as Record<string, unknown> | null;
    if (!d || typeof d !== "object") return null;
    const buckets = Array.isArray(d.buckets) ? d.buckets : [];
    const hours: InfernetHour[] = [];
    for (const b of buckets as Record<string, unknown>[]) {
      const at = Date.parse(String(b.at ?? ""));
      // The operator's share: earned_usd when Infernet names it, else amount_usd.
      const earned = Number(b.earned_usd ?? b.amount_usd);
      if (!Number.isFinite(at) || !Number.isFinite(earned)) continue;
      hours.push({ provider_id: String(b.provider_id ?? ""), at, jobs: Number(b.jobs) || 0, earned_usd: Math.max(0, earned) });
    }
    return { linked: d.linked !== false, hours, providers: Array.isArray(d.providers) ? d.providers.length : 0 };
  } catch {
    return null;
  }
}

// ── the report ───────────────────────────────────────────────────────────────

export interface Dashboard extends Report {
  generated_at: string;
  /** Earliest job this site has on file: history does not reach further back. */
  history_from: number | null;
  claims: (OperatorNode & { online: boolean; on_board_name: string | null })[];
}

export async function dashboardFor(user: { id: string; coinpay_sub: string }, range: RangeKey, now = Date.now()): Promise<Dashboard> {
  const store = earningsStore();
  const { ms, bucketMs } = RANGES[range];
  const from = now - ms;
  const [claims, board, inf, historyFrom] = await Promise.all([
    operatorNodes(user.id),
    getNodesPayload().catch(() => null),
    infernetHours(user.coinpay_sub, from),
    store.firstJobAt().catch(() => null),
  ]);
  const online = new Map<string, NodeRow>((board?.nodes ?? []).map((n) => [n.id, n]));
  // Remember a node's class while it is online, so spend is still priced right when it is not.
  for (const c of claims) {
    const row = online.get(c.node);
    if (row && row.class !== c.class && c.via !== "hosting") {
      c.class = row.class;
      store.update(user.id, c.node, { class: row.class }).catch(() => {});
    }
  }
  const nodes: ReportNode[] = claims.map((c) => ({
    node: c.node,
    label: c.label ?? online.get(c.node)?.name ?? null,
    class: online.get(c.node)?.class ?? c.class ?? "cpu",
    cost_usd_per_hour: c.cost_usd_per_hour,
    online: online.has(c.node),
    via: c.via,
  }));
  const rows = await store.buckets(
    nodes.map((n) => n.node),
    from,
    bucketMs,
  );
  const report = buildReport({ range, now, nodes, rows, infernet: inf });
  return {
    ...report,
    generated_at: new Date(now).toISOString(),
    history_from: historyFrom,
    claims: claims.map((c) => ({ ...c, online: online.has(c.node), on_board_name: online.get(c.node)?.name ?? null })),
  };
}
