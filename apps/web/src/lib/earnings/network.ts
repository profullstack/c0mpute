// Network-wide earnings for /status: proof that running a node pays.
//
// Two ledgers, never double counted:
//  - c0mpute jobs: every credited job copied out of the aggregators into
//    node_jobs (recorder.ts). paid_usd is what the buyer agreed to; the node
//    keeps it minus the market fee (splitPrice, same as /nodes). Infernet chat
//    a c0mpute node serves (type infernet.inference) is ALSO filed there, at
//    the bridge's agreed price; it is paid through Infernet, so those rows are
//    excluded (SETTLED_IN_INFERNET_TYPES) and the ledger counts it once.
//  - Infernet inference: GET /api/v1/network/earnings, the provider_earnings
//    ledger (operator net, house fee, referral commissions), subsidies left out.
// Nothing here names a node, account or wallet.
import { splitPrice } from "../market";

/**
 * Workload types whose pay is settled in Infernet's ledger, not by the c0mpute
 * auction. A chat job Infernet bridges to a c0mpute node is filed twice: once
 * in node_jobs at the price the bridge agreed in the auction, and once as the
 * operator's settled credit in provider_earnings. The ledger is the money that
 * moved, so these node_jobs rows are left out of the c0mpute side.
 */
export const SETTLED_IN_INFERNET_TYPES = ["infernet.inference"] as const;

export const WINDOWS = { "24h": 24 * 3_600_000, "7d": 7 * 24 * 3_600_000, "30d": 30 * 24 * 3_600_000 } as const;
export type WindowKey = keyof typeof WINDOWS | "all";

export interface NodeTotalRow {
  node: string;
  jobs: number;
  paid_usd: number;
}

export interface C0mputeEarnings {
  jobs: number;
  paid_jobs: number;
  /** What buyers agreed to pay, before the market fee. */
  gross_usd: number;
  /** What nodes keep after the market fee. */
  earned_usd: number;
  fee_usd: number;
  earning_nodes: number;
  best_node_usd: number;
  median_node_usd: number;
}

export interface InfernetEarnings {
  earned_usd: number;
  earning_nodes: number;
  jobs: number;
  best_node_usd: number;
  median_node_usd: number;
  house_fee_usd: number;
  house_fee_pct: number;
  billed_usd: number;
  commissions_usd: number;
  earning_referrers: number;
  paid_out_usd?: number;
}

export interface NetworkEarnings {
  generated_at: string;
  /** Earliest c0mpute job on file: c0mpute history does not reach further back. */
  history_from: number | null;
  c0mpute: Record<WindowKey, C0mputeEarnings> | null;
  infernet: Record<WindowKey, InfernetEarnings> | null;
}

const round = (n: number) => Math.round(n * 1e6) / 1e6;

/** Aggregate per-node rows into network totals; node ids never leave this function. */
export function summariseNodeTotals(rows: NodeTotalRow[]): C0mputeEarnings {
  let jobs = 0;
  let gross = 0;
  let paidJobs = 0;
  const kept: number[] = [];
  for (const r of rows) {
    jobs += r.jobs;
    gross += r.paid_usd;
    if (r.paid_usd > 0) {
      paidJobs += r.jobs;
      kept.push(splitPrice(r.paid_usd).operator);
    }
  }
  kept.sort((a, b) => a - b);
  const n = kept.length;
  const median = n === 0 ? 0 : n % 2 ? kept[(n - 1) / 2] : (kept[n / 2 - 1] + kept[n / 2]) / 2;
  const split = splitPrice(gross);
  return {
    jobs,
    paid_jobs: paidJobs,
    gross_usd: round(gross),
    earned_usd: split.operator,
    fee_usd: split.fee,
    earning_nodes: n,
    best_node_usd: kept[n - 1] ?? 0,
    median_node_usd: round(median),
  };
}

const num = (v: unknown) => (Number.isFinite(Number(v)) ? Math.max(0, Number(v)) : 0);

/** Parse Infernet's /api/v1/network/earnings; null when it is missing or malformed. */
export function parseInfernetEarnings(raw: unknown): Record<WindowKey, InfernetEarnings> | null {
  if (!raw || typeof raw !== "object") return null;
  const d = raw as { all_time?: unknown; windows?: Record<string, unknown> };
  const one = (b: unknown): InfernetEarnings | null => {
    if (!b || typeof b !== "object") return null;
    const r = b as Record<string, unknown>;
    return {
      earned_usd: num(r.earned_usd),
      earning_nodes: num(r.earning_nodes),
      jobs: num(r.jobs),
      best_node_usd: num(r.best_node_usd),
      median_node_usd: num(r.median_node_usd),
      house_fee_usd: num(r.house_fee_usd),
      house_fee_pct: num(r.house_fee_pct),
      billed_usd: num(r.billed_usd),
      commissions_usd: num(r.commissions_usd),
      earning_referrers: num(r.earning_referrers),
      ...(r.paid_out_usd !== undefined ? { paid_out_usd: num(r.paid_out_usd) } : {}),
    };
  };
  const all = one(d.all_time);
  if (!all) return null;
  const out = { all } as Record<WindowKey, InfernetEarnings>;
  for (const k of Object.keys(WINDOWS) as (keyof typeof WINDOWS)[]) {
    out[k] = one(d.windows?.[k]) ?? { ...all, earned_usd: 0, jobs: 0, earning_nodes: 0, best_node_usd: 0, median_node_usd: 0, house_fee_usd: 0, billed_usd: 0, commissions_usd: 0, earning_referrers: 0 };
  }
  return out;
}
