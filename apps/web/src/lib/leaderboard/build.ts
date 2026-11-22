// Assemble the public leaderboard from already-read inputs. Pure: no I/O, no
// clock, so the anonymity rules are tested against exactly what ships.
//
// What may appear: public node board ids (as on /nodes), operator nicknames
// operators chose with `c0mpute name`, referral handles from C0MPUTE_REF (as
// on /nodes), model and MCP tool names shared by enough people, and
// "anon-xxxxxx" handles. Never an email, DID, CoinPay sub, wallet, IP,
// account id, API key, prompt or job content. See ./anon.ts.
import { MARKET_FEE_PCT } from "../market";
import {
  MIN_PARTICIPANTS,
  TOP_N,
  cleanLabel,
  foldSmallLabels,
  handleFor,
  roundCount,
  spendBucket,
  type LabelRow,
} from "./anon";

export const WINDOW_KEYS = ["24h", "7d", "30d", "all"] as const;
export type Window = (typeof WINDOW_KEYS)[number];
export const WINDOW_MS: Record<Exclude<Window, "all">, number> = {
  "24h": 24 * 3_600_000,
  "7d": 7 * 24 * 3_600_000,
  "30d": 30 * 24 * 3_600_000,
};

// ── inputs ───────────────────────────────────────────────────────────────────

/** One node on the public board (a subset of NodeRow). */
export interface BoardNode {
  id: string;
  class: string;
  bench_score: number | null;
  name?: string | null;
  last_seen_s: number;
}

/** One node's job history in a window, from the node_jobs ledger. */
export interface NodeWindowStat {
  node: string;
  /** Every job it completed, Infernet-bridged chat included. */
  jobs: number;
  busy_s: number;
  /** Jobs settled by the c0mpute auction itself (Infernet-settled types left out: that ledger counts them). */
  native_jobs: number;
  /** Buyer-agreed pay on those native jobs, before the market fee. */
  native_paid_usd: number;
  /** Distinct UTC days with at least one completed job. */
  active_days: number;
}

/** Infernet's /api/v1/network/leaderboard, one window. Tokens are keyed with our client key. */
export interface InfernetWindow {
  providers: { c0mpute_node?: string; owner?: string | null; provider: string; earned_usd: number; jobs: number }[];
  spenders: { who: string; spent_usd: number; jobs: number }[];
  referrers: { who: string; commissions_usd: number; nodes: number }[];
  models: { model: string; jobs: number; output_tokens: number; users: number }[];
  totals: { spenders: number; spent_usd: number; billed_jobs: number; earning_providers: number; served_jobs: number; output_tokens: number };
}

/** c0mpute.com usage in a window: calls per person token, surface and item (model or MCP tool). */
export interface UsageRow {
  token: string;
  surface: "chat" | "api" | "mcp";
  item: string;
  calls: number;
}

export interface LeaderboardInputs {
  generatedAt: string;
  secret: string | null;
  workersOnline: number;
  nodes: BoardNode[];
  /** C0MPUTE_REF handles and the online nodes each brought, from the aggregator. */
  referralHandles: { ref: string; nodes: number }[];
  nodeStats: Record<Window, NodeWindowStat[]>;
  infernet: Record<Window, InfernetWindow> | null;
  usage: Record<Window, UsageRow[]>;
  /** Network paid to operators per window, the /status figure (both ledgers, counted once). */
  paidToOperators: Record<Window, number> | null;
  /** People who asked to be left out, as person tokens. */
  hiddenTokens: Set<string>;
  /** Nodes whose owner asked to be left out (claimed from the node itself). */
  hiddenNodes: Set<string>;
  history: { node_jobs_from: string | null; usage_from: string | null };
}

// ── output ───────────────────────────────────────────────────────────────────

export interface Board<R> {
  rows: R[];
  /** People (or operators) behind the board in this window, before the top-N cut. */
  participants: number;
  /** Set when the board is held back to keep people anonymous. */
  withheld?: string;
}

export interface NodeRowOut {
  rank: number;
  node: string;
  operator: string | null;
  class: string;
  bench_score: number | null;
  jobs: number;
  busy_hours: number;
  active_days: number;
  online: boolean;
}
export interface EarnerRow {
  rank: number;
  operator: string;
  /** nickname: chosen with `c0mpute name`; node: a public board id; anon: a pseudonymous account. */
  kind: "nickname" | "node" | "anon";
  nodes: number;
  jobs: number;
  earned_usd: number;
}
export interface AffiliateRow {
  rank: number;
  affiliate: string;
  nodes_referred: number;
  commissions_usd: number;
}
export interface UserRow {
  rank: number;
  user: string;
  spend: string;
  billed_jobs: number;
  calls: number;
}
export interface CallerRow {
  rank: number;
  user: string;
  calls: number;
}
export interface LabelOut {
  label: string;
  calls: number;
  users: string;
  output_tokens?: number;
}

export interface WindowBoards {
  nodes: Board<NodeRowOut>;
  earners: Board<EarnerRow>;
  affiliates: Board<AffiliateRow>;
  users: Board<UserRow>;
  api: { models: Board<LabelOut>; callers: Board<CallerRow>; totals: { api_calls: number; chat_calls: number; served_jobs: number; output_tokens: number } };
  agents: { tools: Board<LabelOut>; agents: Board<CallerRow>; totals: { mcp_calls: number } };
}

export interface LeaderboardPayload {
  schema_version: 1;
  generated_at: string;
  windows: readonly Window[];
  headline: {
    workers_online: number;
    jobs_served: Record<Window, number>;
    paid_to_operators_usd: Record<Window, number> | null;
    active_users: Record<Window, number>;
  };
  boards: Record<Window, WindowBoards>;
  /** Public C0MPUTE_REF handles by online nodes referred (as on /nodes), current. */
  referral_handles: { rank: number; handle: string; nodes: number }[];
  history: { node_jobs_from: string | null; usage_from: string | null };
  privacy: { min_participants: number; top_n: number; market_fee_pct: number };
}

const round6 = (n: number) => Math.round(n * 1e6) / 1e6;
const withheldMsg = (n: number) => `fewer than ${MIN_PARTICIPANTS} people in this window (${n === 0 ? "none" : "too few"}): held back to keep them anonymous`;
const usersLabel = (n: number, folded: boolean) => (folded ? `${MIN_PARTICIPANTS}+` : String(n));

function ranked<T>(rows: T[]): (T & { rank: number })[] {
  return rows.slice(0, TOP_N).map((r, i) => ({ ...r, rank: i + 1 }));
}

function pseudoBoard<T>(rows: T[], participants: number, enabled: boolean): Board<T & { rank: number }> {
  if (!enabled) return { rows: [], participants: 0, withheld: "anonymity secret not configured" };
  if (participants < MIN_PARTICIPANTS) return { rows: [], participants: 0, withheld: withheldMsg(participants) };
  return { rows: ranked(rows), participants };
}

function labelBoard(rows: LabelRow[], participants: number): Board<LabelOut> {
  if (participants < MIN_PARTICIPANTS) return { rows: [], participants: 0, withheld: withheldMsg(participants) };
  const folded = foldSmallLabels(rows);
  return {
    rows: folded.slice(0, TOP_N + 1).map((r) => ({
      label: r.label,
      calls: roundCount(r.calls),
      users: usersLabel(r.users, r.label === "other"),
      ...(r.tokens ? { output_tokens: roundCount(r.tokens) } : {}),
    })),
    participants,
  };
}

/** Build the whole payload. */
export function buildLeaderboard(inp: LeaderboardInputs): LeaderboardPayload {
  const secret = inp.secret;
  const anon = (token: string) => handleFor(secret ?? "", token);
  const visibleToken = (t: string | null | undefined) => !!t && !inp.hiddenTokens.has(t);
  const nodeInfo = new Map(inp.nodes.map((n) => [n.id, n]));
  const nickname = (id: string) => {
    const n = nodeInfo.get(id)?.name;
    return typeof n === "string" && /^[a-z0-9_-]{1,32}$/i.test(n) ? n : null;
  };

  const boards = {} as Record<Window, WindowBoards>;
  const jobsServed = {} as Record<Window, number>;
  const activeUsers = {} as Record<Window, number>;

  for (const w of WINDOW_KEYS) {
    const inf = inp.infernet?.[w] ?? null;
    const stats = (inp.nodeStats[w] ?? []).filter((s) => !inp.hiddenNodes.has(s.node));

    // ── nodes: jobs served, then bench ──
    const statByNode = new Map(stats.map((s) => [s.node, s]));
    const nodeIds = new Set<string>([...stats.map((s) => s.node), ...inp.nodes.map((n) => n.id)]);
    const nodeRows = [...nodeIds]
      .filter((id) => !inp.hiddenNodes.has(id))
      .map((id) => {
        const s = statByNode.get(id);
        const n = nodeInfo.get(id);
        return {
          node: id,
          operator: nickname(id),
          class: n?.class ?? "unknown",
          bench_score: n?.bench_score ?? null,
          jobs: s?.jobs ?? 0,
          busy_hours: Math.round(((s?.busy_s ?? 0) / 3600) * 10) / 10,
          active_days: s?.active_days ?? 0,
          online: !!n,
        };
      })
      .filter((r) => r.jobs > 0 || r.bench_score !== null)
      .sort((a, b) => b.jobs - a.jobs || (b.bench_score ?? 0) - (a.bench_score ?? 0) || a.node.localeCompare(b.node));

    // ── earners: an operator is a nickname, else a pseudonymous account, else the node itself ──
    const earners = new Map<string, { operator: string; kind: EarnerRow["kind"]; nodes: Set<string>; jobs: number; earned: number }>();
    const credit = (key: string, operator: string, kind: EarnerRow["kind"], node: string, jobs: number, usd: number) => {
      const e = earners.get(key) ?? { operator, kind, nodes: new Set<string>(), jobs: 0, earned: 0 };
      e.nodes.add(node);
      e.jobs += jobs;
      e.earned += usd;
      earners.set(key, e);
    };
    const nodeOperator = (id: string): [string, string, EarnerRow["kind"]] => {
      const nick = nickname(id);
      return nick ? [`nick:${nick}`, nick, "nickname"] : [`node:${id}`, `node ${id}`, "node"];
    };
    for (const s of stats) {
      if (!(s.native_paid_usd > 0)) continue;
      const [key, op, kind] = nodeOperator(s.node);
      credit(key, op, kind, s.node, s.native_jobs, s.native_paid_usd * (1 - MARKET_FEE_PCT / 100));
    }
    for (const p of inf?.providers ?? []) {
      if (!(p.earned_usd > 0)) continue;
      if (p.c0mpute_node) {
        if (inp.hiddenNodes.has(p.c0mpute_node) || (p.owner && inp.hiddenTokens.has(p.owner))) continue;
        const [key, op, kind] = nodeOperator(p.c0mpute_node);
        credit(key, op, kind, p.c0mpute_node, p.jobs, p.earned_usd);
      } else if (secret) {
        const token = p.owner ?? p.provider;
        if (!visibleToken(token)) continue;
        credit(`anon:${token}`, anon(token), "anon", p.provider, p.jobs, p.earned_usd);
      }
    }
    const earnerRows = [...earners.values()]
      .map((e) => ({ operator: e.operator, kind: e.kind, nodes: e.nodes.size, jobs: e.jobs, earned_usd: round6(e.earned) }))
      .filter((e) => e.earned_usd > 0)
      .sort((a, b) => b.earned_usd - a.earned_usd || b.jobs - a.jobs);

    // ── affiliates: commissions and nodes referred, pseudonymous ──
    const affRows = (inf?.referrers ?? [])
      .filter((r) => visibleToken(r.who))
      .map((r) => ({ affiliate: anon(r.who), nodes_referred: r.nodes, commissions_usd: round6(r.commissions_usd) }))
      .sort((a, b) => b.commissions_usd - a.commissions_usd || b.nodes_referred - a.nodes_referred);

    // ── users: spend (Infernet ledger) + calls on c0mpute.com ──
    const usage = (inp.usage[w] ?? []).filter((u) => visibleToken(u.token));
    const people = new Map<string, { spent: number; billed: number; calls: number; api: number; mcp: number }>();
    const person = (t: string) => {
      if (!people.has(t)) people.set(t, { spent: 0, billed: 0, calls: 0, api: 0, mcp: 0 });
      return people.get(t)!;
    };
    for (const s of inf?.spenders ?? []) {
      if (!visibleToken(s.who)) continue;
      const p = person(s.who);
      p.spent += s.spent_usd;
      p.billed += s.jobs;
    }
    for (const u of usage) {
      const p = person(u.token);
      p.calls += u.calls;
      if (u.surface === "api") p.api += u.calls;
      if (u.surface === "mcp") p.mcp += u.calls;
    }
    const userRows = [...people.entries()]
      .filter(([, p]) => p.spent > 0 || p.calls > 0)
      .sort(([, a], [, b]) => b.spent - a.spent || b.calls - a.calls)
      .map(([t, p]) => ({ user: anon(t), spend: spendBucket(p.spent), billed_jobs: roundCount(p.billed), calls: roundCount(p.calls) }));
    activeUsers[w] = Math.max(userRows.length, inf?.totals.spenders ?? 0);

    // ── API: models served (Infernet ledger) + who calls the c0mpute API ──
    const modelRows: LabelRow[] = (inf?.models ?? []).map((m) => ({ label: cleanLabel(m.model), calls: m.jobs, users: m.users, tokens: m.output_tokens }));
    const modelUsers = Math.max(0, ...(inf?.models ?? []).map((m) => m.users), 0);
    const callerRows = (key: "api" | "mcp") =>
      [...people.entries()]
        .filter(([, p]) => p[key] > 0)
        .sort(([, a], [, b]) => b[key] - a[key])
        .map(([t, p]) => ({ user: anon(t), calls: roundCount(p[key]) }));
    const apiCallers = callerRows("api");
    const sumBy = (s: UsageRow["surface"]) => usage.filter((u) => u.surface === s).reduce((a, u) => a + u.calls, 0);

    // ── agents: MCP tool calls ──
    const toolMap = new Map<string, { calls: number; users: Set<string> }>();
    for (const u of usage) {
      if (u.surface !== "mcp") continue;
      const t = toolMap.get(cleanLabel(u.item)) ?? { calls: 0, users: new Set<string>() };
      t.calls += u.calls;
      t.users.add(u.token);
      toolMap.set(cleanLabel(u.item), t);
    }
    const agentCallers = callerRows("mcp");

    // Jobs served: c0mpute ledger (auction-settled) + Infernet ledger, each job once.
    jobsServed[w] = stats.reduce((a, s) => a + s.native_jobs, 0) + (inf?.totals.served_jobs ?? 0);

    boards[w] = {
      nodes: { rows: ranked(nodeRows), participants: nodeRows.length },
      earners: pseudoBoard(earnerRows, earnerRows.length, true),
      affiliates: pseudoBoard(affRows, affRows.length, !!secret),
      users: pseudoBoard(userRows, userRows.length, !!secret),
      api: {
        models: labelBoard(modelRows, Math.max(modelUsers, inf?.totals.spenders ?? 0)),
        callers: pseudoBoard(apiCallers, apiCallers.length, !!secret),
        totals: {
          api_calls: sumBy("api"),
          chat_calls: sumBy("chat"),
          served_jobs: inf?.totals.served_jobs ?? 0,
          output_tokens: inf?.totals.output_tokens ?? 0,
        },
      },
      agents: {
        tools: labelBoard(
          [...toolMap.entries()].map(([label, t]) => ({ label, calls: t.calls, users: t.users.size })),
          agentCallers.length,
        ),
        agents: pseudoBoard(agentCallers, agentCallers.length, !!secret),
        totals: { mcp_calls: sumBy("mcp") },
      },
    };
  }

  return {
    schema_version: 1,
    generated_at: inp.generatedAt,
    windows: WINDOW_KEYS,
    headline: {
      workers_online: inp.workersOnline,
      jobs_served: jobsServed,
      paid_to_operators_usd: inp.paidToOperators,
      active_users: activeUsers,
    },
    boards,
    referral_handles: inp.referralHandles
      .filter((r) => /^[a-z0-9_-]{1,32}$/.test(r.ref) && r.nodes > 0)
      .sort((a, b) => b.nodes - a.nodes || a.ref.localeCompare(b.ref))
      .slice(0, TOP_N)
      .map((r, i) => ({ rank: i + 1, handle: r.ref, nodes: r.nodes })),
    history: inp.history,
    privacy: { min_participants: MIN_PARTICIPANTS, top_n: TOP_N, market_fee_pct: MARKET_FEE_PCT },
  };
}
