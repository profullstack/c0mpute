// The operator dashboard's numbers, as a pure function of the job history,
// the operator's nodes and what they say each node costs. No I/O here, so the
// arithmetic is tested on its own (report.test.ts).

import { operatorEarnings, powerCostUsd } from "../node-costs";
import type { JobBucketRow } from "./store";

export const RANGES = {
  "1h": { ms: 3_600_000, bucketMs: 300_000, label: "last hour" },
  "3h": { ms: 3 * 3_600_000, bucketMs: 900_000, label: "last 3 hours" },
  "24h": { ms: 24 * 3_600_000, bucketMs: 3_600_000, label: "last 24 hours" },
  "7d": { ms: 7 * 86_400_000, bucketMs: 6 * 3_600_000, label: "last 7 days" },
  "30d": { ms: 30 * 86_400_000, bucketMs: 86_400_000, label: "last 30 days" },
} as const;
export type RangeKey = keyof typeof RANGES;

export function parseRange(raw: string | null | undefined): RangeKey {
  return raw && raw in RANGES ? (raw as RangeKey) : "24h";
}

/** A month as cloud providers bill it, for $/month -> $/hour. */
export const HOURS_PER_MONTH = 730;

/** A node as the report needs it. */
export interface ReportNode {
  node: string;
  label: string | null;
  class: string;
  /** What the operator says it costs to run, $/hour. Null: estimate power from busy time. */
  cost_usd_per_hour: number | null;
  online: boolean;
  via: string;
}

/** Infernet provider earnings, already the operator's share, per hour. */
export interface InfernetHour {
  provider_id: string;
  /** Start of the hour, ms. */
  at: number;
  jobs: number;
  earned_usd: number;
}

export interface Totals {
  jobs: number;
  busy_s: number;
  /** What the operator keeps: c0mpute's share after the house fee, plus Infernet credits. */
  earned_usd: number;
  spend_usd: number;
  net_usd: number;
}

export interface BucketOut extends Totals {
  /** Bucket start, ms. */
  t: number;
}

export interface NodeOut extends Totals {
  node: string;
  label: string | null;
  class: string;
  online: boolean;
  via: string;
  spend_basis: "fixed" | "power-estimate";
  cost_usd_per_hour: number | null;
  /** Busy share of the window, 0..1. */
  utilization: number;
}

export interface TypeOut {
  type: string;
  jobs: number;
  busy_s: number;
  earned_usd: number;
}

export interface Report {
  range: RangeKey;
  from: number;
  to: number;
  bucket_ms: number;
  totals: Totals;
  buckets: BucketOut[];
  nodes: NodeOut[];
  by_type: TypeOut[];
  infernet: { linked: boolean; earned_usd: number; jobs: number; providers: number } | null;
}

const zero = (): Totals => ({ jobs: 0, busy_s: 0, earned_usd: 0, spend_usd: 0, net_usd: 0 });

/**
 * Build the report for `range` ending at `now`.
 *
 * Earned: buyers' agreed price less the house fee (operatorEarnings), plus any
 * Infernet provider credits. Spend: a node with a set cost accrues it for
 * every hour of the window (a rented box bills whether it is busy or not);
 * one without accrues estimated electricity for its busy time only.
 */
export function buildReport(input: {
  range: RangeKey;
  now: number;
  nodes: ReportNode[];
  rows: JobBucketRow[];
  infernet?: { linked: boolean; hours: InfernetHour[]; providers: number } | null;
}): Report {
  const { range, now, nodes } = input;
  const { ms, bucketMs } = RANGES[range];
  // Buckets aligned to the bucket size, covering [from, now]: the first and
  // last can be partial.
  const from = now - ms;
  const firstStart = Math.floor(from / bucketMs) * bucketMs;
  const lastStart = Math.floor((now - 1) / bucketMs) * bucketMs;
  const n = Math.max(1, (lastStart - firstStart) / bucketMs + 1);
  const buckets: BucketOut[] = Array.from({ length: n }, (_, i) => ({ t: firstStart + i * bucketMs, ...zero() }));
  const bucketAt = (t: number) => {
    const i = Math.floor((t - firstStart) / bucketMs);
    return i >= 0 && i < n ? buckets[i] : null;
  };

  const byNode = new Map<string, NodeOut>();
  for (const nd of nodes) {
    byNode.set(nd.node, {
      node: nd.node,
      label: nd.label,
      class: nd.class,
      online: nd.online,
      via: nd.via,
      spend_basis: nd.cost_usd_per_hour != null ? "fixed" : "power-estimate",
      cost_usd_per_hour: nd.cost_usd_per_hour,
      utilization: 0,
      ...zero(),
    });
  }
  const byType = new Map<string, TypeOut>();

  for (const r of input.rows) {
    const nd = byNode.get(r.node);
    if (!nd) continue;
    const b = bucketAt(r.bucket * bucketMs);
    if (!b) continue;
    const earned = operatorEarnings(r.paid_usd);
    const power = nd.cost_usd_per_hour == null ? powerCostUsd(nd.class, r.busy_s) : 0;
    for (const t of [b, nd]) {
      t.jobs += r.jobs;
      t.busy_s += r.busy_s;
      t.earned_usd += earned;
      t.spend_usd += power;
    }
    const ty = byType.get(r.type) ?? { type: r.type, jobs: 0, busy_s: 0, earned_usd: 0 };
    ty.jobs += r.jobs;
    ty.busy_s += r.busy_s;
    ty.earned_usd += earned;
    byType.set(r.type, ty);
  }

  // Fixed running costs: each bucket's overlap with the window, up to now.
  for (const b of buckets) {
    const hours = Math.max(0, Math.min(b.t + bucketMs, now) - Math.max(b.t, from)) / 3_600_000;
    for (const nd of byNode.values()) {
      if (nd.cost_usd_per_hour == null) continue;
      const c = nd.cost_usd_per_hour * hours;
      b.spend_usd += c;
      nd.spend_usd += c;
    }
  }

  // Infernet credits come per hour: spread each hour over the buckets it overlaps.
  let infernet: Report["infernet"] = null;
  if (input.infernet) {
    let earned = 0;
    let jobs = 0;
    for (const h of input.infernet.hours) {
      const hEnd = h.at + 3_600_000;
      for (const b of buckets) {
        const lo = Math.max(h.at, b.t, from);
        const hi = Math.min(hEnd, b.t + bucketMs, now);
        if (hi <= lo) continue;
        const f = (hi - lo) / 3_600_000;
        b.earned_usd += h.earned_usd * f;
        b.jobs += h.jobs * f;
        earned += h.earned_usd * f;
        jobs += h.jobs * f;
      }
    }
    infernet = { linked: input.infernet.linked, earned_usd: earned, jobs: Math.round(jobs), providers: input.infernet.providers };
    if (earned > 0 || jobs > 0) {
      byType.set("infernet.provider", { type: "infernet.provider", jobs: Math.round(jobs), busy_s: 0, earned_usd: earned });
    }
  }

  const totals = zero();
  for (const b of buckets) {
    b.jobs = Math.round(b.jobs);
    b.net_usd = b.earned_usd - b.spend_usd;
    totals.jobs += b.jobs;
    totals.busy_s += b.busy_s;
    totals.earned_usd += b.earned_usd;
    totals.spend_usd += b.spend_usd;
  }
  totals.net_usd = totals.earned_usd - totals.spend_usd;

  const windowS = Math.max(1, (now - from) / 1000);
  const nodesOut = [...byNode.values()].map((nd) => ({
    ...nd,
    net_usd: nd.earned_usd - nd.spend_usd,
    utilization: Math.min(1, nd.busy_s / windowS),
  }));
  nodesOut.sort((a, b) => b.earned_usd - a.earned_usd || b.jobs - a.jobs || a.node.localeCompare(b.node));

  return {
    range,
    from,
    to: now,
    bucket_ms: bucketMs,
    totals,
    buckets,
    nodes: nodesOut,
    by_type: [...byType.values()].sort((a, b) => b.earned_usd - a.earned_usd || b.jobs - a.jobs),
    infernet,
  };
}

/** Parse what an operator typed for a node's running cost into $/hour, or null to clear it. */
export function parseCost(amount: unknown, per: unknown): number | null | "invalid" {
  if (amount === null || amount === "" || amount === undefined) return null;
  const v = typeof amount === "number" ? amount : Number(String(amount).trim().replace(/^\$/, ""));
  if (!Number.isFinite(v) || v < 0) return "invalid";
  const perHour = per === "month" ? v / HOURS_PER_MONTH : per === "hour" || per === undefined ? v : null;
  if (perHour === null || perHour > 1_000) return "invalid";
  return perHour;
}
