// The operator dashboard's numbers, as a pure function of the job history,
// the operator's nodes, what they say each node costs, and the settled pay
// Infernet credited. No I/O here, so the arithmetic is tested on its own.
//
// What counts as money:
// - earned = settled credits in Infernet's provider_earnings ledger (the
//   operator's net, after the house fee). A c0mpute node that serves chat
//   jobs bridged from Infernet is paid there, through the provider Infernet
//   opened for it (`c0mpute:<peer>`), at Infernet's price.
// - Of that, credits paid by the operator's own account (or, for the
//   platform, by the platform's) are "your own jobs": money that went round
//   in a circle. The headline and net count only what others paid.
// - agreed = the price in the c0mpute auction, as the aggregators heard it.
//   It is an offer, not a payment: shown for reference, never as income.

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

/** Settled Infernet credits for one provider and hour, the operator's net. */
export interface InfernetHour {
  provider_id: string;
  /** The c0mpute board id when the provider stands for a c0mpute node. */
  c0mpute_node: string | null;
  /** Start of the hour, ms. */
  at: number;
  jobs: number;
  earned_usd: number;
  /** The part paid by the operator's own account (or the platform's). */
  own_jobs: number;
  own_usd: number;
}

export interface InfernetInput {
  linked: boolean;
  hours: InfernetHour[];
  providers: { id: string; name: string | null; c0mpute_node: string | null }[];
}

export interface Totals {
  /** Jobs completed on c0mpute (auctions the aggregators heard). */
  jobs: number;
  busy_s: number;
  /** Settled pay from others: the operator's net. The headline. */
  earned_usd: number;
  /** Settled jobs paid by others. */
  paid_jobs: number;
  /** Settled pay for jobs the operator (or the platform) paid for itself. */
  own_usd: number;
  own_jobs: number;
  /** c0mpute auction prices less the fee: offered, not settled. */
  agreed_usd: number;
  spend_usd: number;
  /** earned_usd - spend_usd: own jobs are not profit. */
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
  /** "infernet" rows are Infernet providers that are not c0mpute nodes. */
  kind: "c0mpute" | "infernet";
  spend_basis: "fixed" | "power-estimate" | "none";
  cost_usd_per_hour: number | null;
  /** Busy share of the window, 0..1. */
  utilization: number;
}

export interface TypeOut {
  type: string;
  jobs: number;
  busy_s: number;
  agreed_usd: number;
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
  infernet: { linked: boolean; providers: number } | null;
}

const zero = (): Totals => ({
  jobs: 0,
  busy_s: 0,
  earned_usd: 0,
  paid_jobs: 0,
  own_usd: 0,
  own_jobs: 0,
  agreed_usd: 0,
  spend_usd: 0,
  net_usd: 0,
});

/**
 * Build the report for `range` ending at `now`.
 *
 * Spend: a node with a set cost accrues it for every hour of the window (a
 * rented box bills whether it is busy or not); one without accrues estimated
 * electricity for its busy time only.
 */
export function buildReport(input: { range: RangeKey; now: number; nodes: ReportNode[]; rows: JobBucketRow[]; infernet?: InfernetInput | null }): Report {
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
      kind: "c0mpute",
      spend_basis: nd.cost_usd_per_hour != null ? "fixed" : "power-estimate",
      cost_usd_per_hour: nd.cost_usd_per_hour,
      utilization: 0,
      ...zero(),
    });
  }
  const byType = new Map<string, TypeOut>();

  // c0mpute auctions: jobs, busy time, power estimate, and the agreed price (reference only).
  for (const r of input.rows) {
    const nd = byNode.get(r.node);
    if (!nd) continue;
    const b = bucketAt(r.bucket * bucketMs);
    if (!b) continue;
    const agreed = operatorEarnings(r.paid_usd);
    const power = nd.cost_usd_per_hour == null ? powerCostUsd(nd.class, r.busy_s) : 0;
    for (const t of [b, nd]) {
      t.jobs += r.jobs;
      t.busy_s += r.busy_s;
      t.agreed_usd += agreed;
      t.spend_usd += power;
    }
    const ty = byType.get(r.type) ?? { type: r.type, jobs: 0, busy_s: 0, agreed_usd: 0 };
    ty.jobs += r.jobs;
    ty.busy_s += r.busy_s;
    ty.agreed_usd += agreed;
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

  // Settled pay: Infernet credits per hour, spread over the buckets each hour overlaps.
  let infernet: Report["infernet"] = null;
  if (input.infernet) {
    const names = new Map(input.infernet.providers.map((p) => [p.id, p]));
    for (const h of input.infernet.hours) {
      // A c0mpute node's credits land on its row; an Infernet provider gets a row of its own.
      let target = h.c0mpute_node ? byNode.get(h.c0mpute_node) : undefined;
      if (!target) {
        const key = `infernet:${h.provider_id}`;
        target = byNode.get(key);
        if (!target) {
          const p = names.get(h.provider_id);
          target = {
            node: key,
            label: p?.name ?? h.c0mpute_node ?? "Infernet provider",
            class: "infernet",
            online: true,
            via: "infernet",
            kind: "infernet",
            spend_basis: "none",
            cost_usd_per_hour: null,
            utilization: 0,
            ...zero(),
          };
          byNode.set(key, target);
        }
      }
      const hEnd = h.at + 3_600_000;
      const othersUsd = Math.max(0, h.earned_usd - h.own_usd);
      const othersJobs = Math.max(0, h.jobs - h.own_jobs);
      for (const b of buckets) {
        const lo = Math.max(h.at, b.t, from);
        const hi = Math.min(hEnd, b.t + bucketMs, now);
        if (hi <= lo) continue;
        const f = (hi - lo) / 3_600_000;
        for (const t of [b, target]) {
          t.earned_usd += othersUsd * f;
          t.paid_jobs += othersJobs * f;
          t.own_usd += h.own_usd * f;
          t.own_jobs += h.own_jobs * f;
        }
      }
    }
    infernet = { linked: input.infernet.linked, providers: input.infernet.providers.length };
  }

  const finish = (t: Totals) => {
    t.paid_jobs = Math.round(t.paid_jobs);
    t.own_jobs = Math.round(t.own_jobs);
    t.net_usd = t.earned_usd - t.spend_usd;
  };
  // Sum before rounding job counts: an hour spread over four bars would
  // otherwise round 2.5 jobs up four times.
  const totals = zero();
  for (const b of buckets) {
    for (const k of Object.keys(totals) as (keyof Totals)[]) totals[k] += b[k];
    finish(b);
  }
  finish(totals);

  const windowS = Math.max(1, (now - from) / 1000);
  const nodesOut = [...byNode.values()]
    // Infernet-only providers that settled nothing in the window are noise.
    .filter((nd) => nd.kind === "c0mpute" || nd.earned_usd > 0 || nd.own_usd > 0)
    .map((nd) => {
      finish(nd);
      return { ...nd, utilization: Math.min(1, nd.busy_s / windowS) };
    });
  nodesOut.sort(
    (a, b) => b.earned_usd - a.earned_usd || b.own_usd - a.own_usd || b.jobs - a.jobs || a.node.localeCompare(b.node),
  );

  return {
    range,
    from,
    to: now,
    bucket_ms: bucketMs,
    totals,
    buckets,
    nodes: nodesOut,
    by_type: [...byType.values()].sort((a, b) => b.jobs - a.jobs || b.agreed_usd - a.agreed_usd),
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
