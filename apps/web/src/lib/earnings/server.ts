// Server wiring for the operator dashboard: whose nodes are whose, Infernet
// provider credits, and the report. The recorder lives in ./recorder.
import "server-only";
import { infernet } from "../chat/server";
import { getNodesPayload, type NodeRow } from "../nodes";
import { buildReport, RANGES, type InfernetHour, type InfernetInput, type RangeKey, type Report, type ReportNode } from "./report";
import type { OperatorNode } from "./store";
import { earningsStore } from "./recorder";
import { buildObserved, type ObservedWindow } from "./network-by-class";

export { earningsStore };

export { operatorNodes } from "./operators";
import { operatorNodes } from "./operators";

// ── Infernet ─────────────────────────────────────────────────────────────────

/**
 * This person's settled Infernet credits per provider and hour since `since`,
 * read through our trusted client key by their CoinPay subject. `c0mputeNodes`
 * are the board ids this account claims: Infernet adds the providers it
 * opened for them (`c0mpute:<peer>`), which is where a bridged job is paid.
 * Null when Infernet is unreachable.
 */
export async function infernetHours(subject: string, since: number, c0mputeNodes: string[] = []): Promise<InfernetInput | null> {
  try {
    const q = new URLSearchParams({ since: new Date(since).toISOString() });
    if (c0mputeNodes.length) q.set("c0mpute_nodes", c0mputeNodes.slice(0, 200).join(","));
    const r = await infernet(`/api/v1/billing/node-earnings?${q}`, { signal: AbortSignal.timeout(8_000), cache: "no-store" }, { subject });
    if (!r.ok) return null;
    const raw = (await r.json()) as { data?: unknown };
    const d = (raw && typeof raw === "object" && "data" in raw ? raw.data : raw) as Record<string, unknown> | null;
    if (!d || typeof d !== "object") return null;
    const providers = (Array.isArray(d.providers) ? d.providers : []).map((p: Record<string, unknown>) => ({
      id: String(p.id ?? ""),
      name: typeof p.name === "string" ? p.name : null,
      c0mpute_node: typeof p.c0mpute_node === "string" ? p.c0mpute_node : null,
    }));
    const nodeOf = new Map(providers.map((p) => [p.id, p.c0mpute_node]));
    const num = (v: unknown) => (Number.isFinite(Number(v)) ? Math.max(0, Number(v)) : 0);
    const hours: InfernetHour[] = [];
    for (const b of (Array.isArray(d.buckets) ? d.buckets : []) as Record<string, unknown>[]) {
      const at = Date.parse(String(b.at ?? ""));
      if (!Number.isFinite(at)) continue;
      const pid = String(b.provider_id ?? "");
      // amount_usd is the operator's net (after the house fee).
      const earned = num(b.amount_usd);
      hours.push({
        provider_id: pid,
        c0mpute_node: nodeOf.get(pid) ?? null,
        at,
        jobs: num(b.jobs),
        earned_usd: earned,
        own_jobs: num(b.own_jobs),
        own_usd: Math.min(earned, num(b.own_usd)),
      });
    }
    return { linked: d.linked !== false, hours, providers };
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

export async function dashboardFor(user: { id: string; coinpay_sub: string | null }, range: RangeKey, now = Date.now()): Promise<Dashboard> {
  const store = earningsStore();
  const { ms, bucketMs } = RANGES[range];
  const from = now - ms;
  const claims = await operatorNodes(user.id);
  const [board, inf, historyFrom] = await Promise.all([
    getNodesPayload().catch(() => null),
    // Infernet credits are read by CoinPay subject: none until CoinPay is connected.
    user.coinpay_sub
      ? infernetHours(
          user.coinpay_sub,
          from,
          claims.map((c) => c.node),
        )
      : Promise.resolve(null),
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

// ── job prices by hardware class, for /estimates ────────────────────────────────────────

export interface NetworkObserved {
  generated_at: string;
  history_from: number | null;
  windows: ObservedWindow[];
}

/** Observed agreed prices across every node over the last 7 and 30 days. Null when the history is unreadable. */
export async function networkObserved(now = Date.now()): Promise<NetworkObserved | null> {
  try {
    const store = earningsStore();
    const [rows30, rows7, claimed, board, historyFrom] = await Promise.all([
      store.networkSince(now - 30 * 86_400_000),
      store.networkSince(now - 7 * 86_400_000),
      store.claimedClasses().catch(() => new Map<string, string>()),
      getNodesPayload().catch(() => null),
      store.firstJobAt().catch(() => null),
    ]);
    const online = new Map<string, string>((board?.nodes ?? []).map((n) => [n.id, n.class]));
    const classOf = (node: string) => online.get(node) ?? claimed.get(node) ?? null;
    return {
      generated_at: new Date(now).toISOString(),
      history_from: historyFrom,
      windows: [
        buildObserved({ days: 7, rows: rows7, classOf, historyFrom, now }),
        buildObserved({ days: 30, rows: rows30, classOf, historyFrom, now }),
      ],
    };
  } catch (e) {
    console.error("estimates: network history failed", e);
    return null;
  }
}
