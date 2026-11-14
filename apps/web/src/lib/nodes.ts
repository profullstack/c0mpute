import "server-only";
import { aggregatorUrls } from "./aggregators";
import { OPERATOR_SHARE_PCT } from "./market";

/**
 * One row of the public node board, as served by the aggregator's `GET /nodes`.
 *
 * Public fields only: short peer id, class, cores, bench, version, ref. Never
 * the CPU model string (it fingerprints an operator's machine); older
 * aggregators still send `cpu`, so `publicRow` strips it.
 */
export interface NodeRow {
  id: string;
  class: string;
  cores: number | null;
  ram_bytes: number | null;
  bench_score: number | null;
  bench_at: string | null;
  benchmarks: Record<string, WorkloadBenchmark> | null;
  version: string | null;
  ref: string | null;
  /** The operator's nickname (`c0mpute name`); absent on nodes older than 0.2.64. */
  name?: string | null;
  /** The operator's listing; absent on nodes older than 0.2.35. */
  pricing?: Pricing | null;
  /**
   * The node's gateway origin (`http://host:7777`), only when its operator set
   * `gateway.announce`. Clients that are not libp2p peers find gateways that
   * serve hosted sites (`/s/<site_id>/`) here. Absent on older aggregators.
   */
  gateway?: string | null;
  /** Projects the node donates idle compute to (e.g. `crowdgpt`); absent when none. */
  donates?: string[];
  last_seen_s: number;
  /**
   * One identity running as more than one process (a copied identity dir, or
   * two services on one box). `instances`: processes the aggregator heard;
   * `reported`: the node detected it itself; `yielding`: this is the older
   * process and it stopped bidding. Absent when there is no duplicate.
   */
  duplicate?: { instances: number; reported: boolean; yielding: boolean } | null;
  /**
   * Jobs this node completed in the last 24h as the aggregator heard them on
   * the job topics: run time (accept to receipt) and what buyers agreed to pay,
   * before the fee, by workload type. Absent when it completed none, and from
   * aggregators older than this field.
   */
  jobs_24h?: NodeJobs | null;
}

export interface WorkloadJobs {
  jobs: number;
  busy_s: number;
  paid_usd: number;
}

export interface NodeJobs extends WorkloadJobs {
  by_type: Record<string, WorkloadJobs>;
}

export interface BenchSample {
  threads: number;
  duration_us: number;
  speedup: number;
  throughput: number;
}

export interface WorkloadBenchmark {
  params: string;
  samples: BenchSample[];
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

/**
 * Rough power draw while a job runs, by board class. Only used for the
 * "est. power" column: the board knows the class, not the machine, so this is
 * a typical figure for a desktop under load, not a measurement.
 */
export const BUSY_WATTS: Record<string, number> = {
  cpu: 65,
  "gpu:apple": 40,
  "gpu:nvidia": 250,
  "gpu:amd": 250,
};

/** Electricity price used for the estimate (about the US residential average). */
export const POWER_USD_PER_KWH = 0.17;

/** Estimated electricity spent on `busySeconds` of work on a node of `cls`. */
export function powerCostUsd(cls: string, busySeconds: number): number {
  const watts = BUSY_WATTS[cls] ?? BUSY_WATTS.cpu;
  return (watts / 1000) * (busySeconds / 3600) * POWER_USD_PER_KWH;
}

/** What the operator keeps from what buyers paid: the share after the house fee. */
export function operatorEarnings(paidUsd: number): number {
  return (paidUsd * OPERATOR_SHARE_PCT) / 100;
}

/** One row of the network-wide "by job type" table. */
export interface WorkloadSummary extends WorkloadJobs {
  type: string;
  nodes: number;
}

/** Sum every node's 24h jobs by workload type, busiest type first. */
export function summarizeByType(nodes: NodeRow[]): WorkloadSummary[] {
  const by = new Map<string, WorkloadSummary>();
  for (const n of nodes) {
    for (const [type, w] of Object.entries(n.jobs_24h?.by_type ?? {})) {
      const row = by.get(type) ?? { type, jobs: 0, busy_s: 0, paid_usd: 0, nodes: 0 };
      row.jobs += w.jobs;
      row.busy_s += w.busy_s;
      row.paid_usd += w.paid_usd;
      row.nodes += 1;
      by.set(type, row);
    }
  }
  return [...by.values()].sort((a, b) => b.jobs - a.jobs || a.type.localeCompare(b.type));
}

/** "$0.0042" for sub-cent amounts, "$1.23" otherwise, "$0" for nothing. */
export function usd(n: number): string {
  if (n === 0) return "$0";
  const sign = n < 0 ? "-" : "";
  const a = Math.abs(n);
  return a < 0.01 ? `${sign}$${a.toPrecision(2)}` : `${sign}$${a.toFixed(2)}`;
}

/** "42s" / "3.5m" / "2.1h". */
export function duration(seconds: number): string {
  if (seconds < 90) return `${Math.round(seconds)}s`;
  if (seconds < 5400) return `${(seconds / 60).toFixed(1)}m`;
  return `${(seconds / 3600).toFixed(1)}h`;
}

export interface NodesPayload {
  generated_at: string;
  workers_online: number;
  nodes: NodeRow[];
  /** Nodes online per nickname; absent from aggregators older than 0.2.64. */
  operators: { name: string; nodes: number }[];
  referrers: { ref: string; nodes: number }[];
  source: "aggregator" | "unavailable";
}

/** The mission number. A goal, not a count. */
export const NODE_GOAL = 100_000;

/** Keep only the public fields of a row; drops `cpu` from older aggregators. */
export function publicRow(row: NodeRow & { cpu?: unknown }): NodeRow {
  const { cpu, ...rest } = row;
  void cpu;
  return rest;
}

type RawNodes = Omit<NodesPayload, "source">;

/**
 * Overlay each node's 24h job stats from every aggregator onto `base`, taking
 * for each node the answer that counted the most jobs. Each aggregator keeps
 * its 24h window in memory, so one restarted by a deploy starts at zero while
 * another still holds the whole day; the board should show the fuller one.
 */
export function mergeJobStats(base: NodeRow[], answers: RawNodes[]): NodeRow[] {
  const best = new Map<string, NodeJobs>();
  for (const a of answers) {
    for (const n of Array.isArray(a.nodes) ? a.nodes : []) {
      const j = n.jobs_24h;
      if (!j || !(j.jobs > 0)) continue;
      const cur = best.get(n.id);
      if (!cur || j.jobs > cur.jobs) best.set(n.id, j);
    }
  }
  return base.map((n) => {
    const j = best.get(n.id);
    return j ? { ...n, jobs_24h: j } : n;
  });
}

async function fetchNodesFrom(base: string): Promise<RawNodes | null> {
  try {
    const r = await fetch(`${base}/nodes`, { signal: AbortSignal.timeout(2_000), cache: "no-store" });
    return r.ok ? ((await r.json()) as RawNodes) : null;
  } catch {
    return null;
  }
}

export async function getNodesPayload(): Promise<NodesPayload> {
  const answers = (await Promise.all(aggregatorUrls().map(fetchNodesFrom))).filter(
    (a): a is RawNodes => a !== null && typeof a === "object",
  );
  // The board itself comes from the aggregator that sees the most workers.
  const data = answers.reduce<RawNodes | null>(
    (b, a) => (b === null || (a.workers_online ?? 0) > (b.workers_online ?? 0) ? a : b),
    null,
  );
  if (data) {
    return {
      ...data,
      nodes: mergeJobStats(Array.isArray(data.nodes) ? data.nodes.map(publicRow) : [], answers),
      operators: Array.isArray(data.operators) ? data.operators : [],
      referrers: Array.isArray(data.referrers) ? data.referrers : [],
      source: "aggregator",
    };
  }
  return {
    generated_at: new Date().toISOString(),
    workers_online: 0,
    nodes: [],
    operators: [],
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
