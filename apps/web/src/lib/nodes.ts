import "server-only";
import { fetchFromAggregators } from "./aggregators";

/** One row of the public node board, as served by the aggregator's `GET /nodes`. */
export interface NodeRow {
  id: string;
  class: string;
  cores: number | null;
  bench_score: number | null;
  version: string | null;
  ref: string | null;
  /** The operator's listing; absent on nodes older than 0.2.35. */
  pricing?: Pricing | null;
  last_seen_s: number;
}

/** How a node prices its bids, set by its operator with `c0mpute pricing set`. */
export interface Pricing {
  mode: "dynamic" | "fixed";
  min_usd: number;
  fixed_usd?: number;
}

/** "dynamic · min $0.02" / "fixed $0.30" / "-" for the market table. */
export function priceLabel(p: Pricing | null | undefined): string {
  if (!p) return "-";
  const usd = (n: number) => `$${n < 0.01 && n > 0 ? n : n.toFixed(2)}`;
  if (p.mode === "fixed" && p.fixed_usd != null) {
    return `fixed ${usd(Math.max(p.fixed_usd, p.min_usd))}`;
  }
  return p.min_usd > 0 ? `dynamic · min ${usd(p.min_usd)}` : "dynamic";
}

export interface NodesPayload {
  generated_at: string;
  workers_online: number;
  nodes: NodeRow[];
  referrers: { ref: string; nodes: number }[];
  source: "aggregator" | "unavailable";
}

/** The mission number. A goal, not a count. */
export const NODE_GOAL = 100_000;

export async function getNodesPayload(): Promise<NodesPayload> {
  const data = await fetchFromAggregators<Omit<NodesPayload, "source">>("/nodes");
  if (data) return { ...data, source: "aggregator" };
  return {
    generated_at: new Date().toISOString(),
    workers_online: 0,
    nodes: [],
    referrers: [],
    source: "unavailable",
  };
}

/** A referral code as the worker accepts it: lowercase [a-z0-9_-], 1 to 32 chars. */
export function sanitizeRef(raw: string | null | undefined): string | null {
  if (!raw) return null;
  const code = raw.trim().toLowerCase();
  return /^[a-z0-9_-]{1,32}$/.test(code) ? code : null;
}

export function installLine(ref?: string | null): string {
  const code = sanitizeRef(ref);
  return code
    ? `curl -fsSL https://c0mpute.com/install.sh | C0MPUTE_REF=${code} sh`
    : "curl -fsSL https://c0mpute.com/install.sh | sh";
}
