// An operator's fleet view (nodes, status, earnings, queued commands, what each
// node reported), shared by the routes (accounts/server.ts fleetFor) and the
// fleet alerts scheduler. No "server-only" import: instrumentation.ts loads the
// scheduler.

import type { NodesPayload } from "../nodes-core";
import { getNodesPayload } from "../nodes-core";
import { operatorNodes } from "../earnings/operators";
import type { EarningsStore } from "../earnings/store";
import { buildFleet, type FleetService } from "./fleet";
import type { AccountStore } from "./store";

const DAY = 86_400_000;

/**
 * The fleet of one account. `board` lets a caller that checks many accounts
 * read the node board once; `boardOk` says whether it could be read at all
 * (a failed read makes every node look offline, which alerts must not trust).
 */
export async function fleetView(
  userId: string,
  d: { accounts: AccountStore; fleet: FleetService; earnings: EarningsStore; board?: NodesPayload | null; now?: number },
) {
  const claims = await operatorNodes(userId);
  const nodes = claims.map((c) => c.node);
  const now = d.now ?? Date.now();
  const board = d.board !== undefined ? d.board : await getNodesPayload().catch(() => null);
  const [meta, t24, t30, tAll, state] = await Promise.all([
    d.accounts.nodeMeta(userId),
    d.earnings.totalsFor(nodes, now - DAY),
    d.earnings.totalsFor(nodes, now - 30 * DAY),
    d.earnings.totalsFor(nodes, 0),
    d.fleet.state(userId),
  ]);
  const boardOk = !!board && board.source === "aggregator" && Array.isArray(board.nodes) && board.nodes.length > 0;
  return { ...buildFleet({ claims, meta, board: board?.nodes ?? [], t24, t30, tAll, pending: state.pending, paused: state.paused, states: state.states }), boardOk };
}

/** Earned in the last 24h, and the daily average of the 7 days before it. */
export async function earningsTrend(earnings: EarningsStore, nodes: string[], now = Date.now()): Promise<{ h24: number; avg7: number }> {
  const sum = (m: Map<string, { paid_usd: number }>) => [...m.values()].reduce((s, v) => s + v.paid_usd, 0);
  const [d1, d8] = await Promise.all([earnings.totalsFor(nodes, now - DAY), earnings.totalsFor(nodes, now - 8 * DAY)]);
  const h24 = sum(d1);
  return { h24, avg7: Math.max(0, sum(d8) - h24) / 7 };
}
