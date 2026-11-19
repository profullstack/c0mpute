// /dashboard/disk numbers as a pure function of the recorded hourly dues,
// whose nodes and sites are whose, and the live hosting picture. No I/O.
//
// - earned: dues on sites someone else pays for, held by your nodes.
// - spend:  dues on your sites, held by someone else's nodes.
// - own:    your sites on your own nodes: money in a circle, neither.
// All "accrued": owed by the settlement rule; the payer's node pays it from
// its own wallet (or, with [payments] off, only logs it).

import { RANGES, type RangeKey } from "../earnings/report";
import type { DueRow } from "./store";

const HOUR = 3_600_000;
const GIB = 2 ** 30;

export interface DiskTotals {
  earned_usd: number;
  spend_usd: number;
  own_usd: number;
  net_usd: number;
}

export interface DiskBucket extends DiskTotals {
  t: number;
}

/** A hosted site as a gateway's `/hosting/v1/sites` lists it. */
export interface LiveSite {
  site_id: string;
  name: string;
  owner: string;
  size: number;
  copies: number;
  price_per_gib_month: string | null;
  preferred_hosts: string[];
}

/** One of your sites, how you got it on the list. */
export interface MySite {
  site_id: string;
  via: "push" | "claim" | "owner";
  name?: string | null;
  status?: string | null;
  url?: string | null;
}

export interface SiteOut {
  site_id: string;
  name: string;
  url: string;
  via: MySite["via"];
  /** Push status from c0mpute chat, when it came from there. */
  status: string | null;
  live: boolean;
  size_bytes: number | null;
  copies: number | null;
  /** $/GiB-month, null for a free site. */
  price_per_gib_month: number | null;
  /** Nodes paid for holding it in the newest recorded hour. */
  holders: number;
  /** What it costs a month at the newest hour's rate (accrued). */
  monthly_usd: number;
  spend_usd: number;
  own_usd: number;
}

export interface NodeDiskOut {
  node: string;
  label: string | null;
  online: boolean;
  /** Paid bytes held in the newest recorded hour. */
  held_bytes: number;
  sites: number;
  earned_usd: number;
  own_usd: number;
  /** Challenges passed / asked in the window: below half pays nothing. */
  passed: number;
  challenged: number;
  offer: { free_bytes: number; total_bytes: number; per_gib_month: number } | null;
}

export interface DiskReport {
  range: RangeKey;
  from: number;
  to: number;
  bucket_ms: number;
  totals: DiskTotals;
  buckets: DiskBucket[];
  sites: SiteOut[];
  nodes: NodeDiskOut[];
  /** Newest hour on file in the window, ms. */
  latest_hour: number | null;
}

const zero = (): DiskTotals => ({ earned_usd: 0, spend_usd: 0, own_usd: 0, net_usd: 0 });

export function buildDiskReport(input: {
  range: RangeKey;
  now: number;
  rows: DueRow[];
  nodes: { node: string; label: string | null; online: boolean; offer?: NodeDiskOut["offer"] }[];
  sites: MySite[];
  owners: string[];
  live: LiveSite[];
  gateway?: string;
}): DiskReport {
  const { range, now } = input;
  const { ms, bucketMs } = RANGES[range];
  const from = now - ms;
  const firstStart = Math.floor(from / bucketMs) * bucketMs;
  const lastStart = Math.floor((now - 1) / bucketMs) * bucketMs;
  const n = Math.max(1, (lastStart - firstStart) / bucketMs + 1);
  const buckets: DiskBucket[] = Array.from({ length: n }, (_, i) => ({ t: firstStart + i * bucketMs, ...zero() }));
  const gateway = (input.gateway ?? "https://gateway.c0mpute.com").replace(/\/+$/, "");

  const myNodes = new Set(input.nodes.map((x) => x.node));
  const owners = new Set(input.owners);
  const live = new Map(input.live.map((s) => [s.site_id, s]));
  const mine = new Map<string, MySite>();
  for (const s of input.sites) if (!mine.has(s.site_id)) mine.set(s.site_id, s);
  // Every live site of a claimed owner key is yours too.
  for (const s of input.live) if (owners.has(s.owner) && !mine.has(s.site_id)) mine.set(s.site_id, { site_id: s.site_id, via: "owner" });
  for (const r of input.rows) if (owners.has(r.owner) && !mine.has(r.site_id)) mine.set(r.site_id, { site_id: r.site_id, via: "owner", name: r.name });

  const nodeOut = new Map<string, NodeDiskOut>(
    input.nodes.map((x) => [
      x.node,
      { node: x.node, label: x.label, online: x.online, held_bytes: 0, sites: 0, earned_usd: 0, own_usd: 0, passed: 0, challenged: 0, offer: x.offer ?? null },
    ]),
  );
  const siteMoney = new Map<string, { spend: number; own: number }>();
  const totals = zero();

  const rows = input.rows.filter((r) => r.hour + HOUR > from && r.hour < now);
  const latest = rows.reduce<number | null>((m, r) => (m === null || r.hour > m ? r.hour : m), null);
  const latestHolders = new Map<string, Set<string>>();
  const latestMonthly = new Map<string, number>();

  for (const r of rows) {
    const usd = r.amount_micros / 1e6;
    const nodeMine = myNodes.has(r.node);
    const siteMine = mine.has(r.site_id);
    if (!nodeMine && !siteMine) continue;
    const kind: keyof DiskTotals = nodeMine && siteMine ? "own_usd" : nodeMine ? "earned_usd" : "spend_usd";
    // Spread the hour over the buckets it overlaps (5-minute bars on 1h).
    for (const b of buckets) {
      const lo = Math.max(r.hour, b.t, from);
      const hi = Math.min(r.hour + HOUR, b.t + bucketMs, now);
      if (hi <= lo) continue;
      const v = (usd * (hi - lo)) / HOUR;
      b[kind] += v;
      totals[kind] += v;
      if (nodeMine) {
        const nd = nodeOut.get(r.node)!;
        if (kind === "own_usd") nd.own_usd += v;
        else nd.earned_usd += v;
      }
      if (siteMine) {
        const m = siteMoney.get(r.site_id) ?? { spend: 0, own: 0 };
        if (kind === "own_usd") m.own += v;
        else m.spend += v;
        siteMoney.set(r.site_id, m);
      }
    }
    if (nodeMine) {
      const nd = nodeOut.get(r.node)!;
      nd.passed += r.passed;
      nd.challenged += r.challenged;
      if (r.hour === latest) {
        nd.held_bytes += r.bytes;
        nd.sites += 1;
      }
    }
    if (siteMine && r.hour === latest) {
      const hs = latestHolders.get(r.site_id) ?? new Set<string>();
      hs.add(r.holder);
      latestHolders.set(r.site_id, hs);
      // A month at this holder's rate: price × GiB held, whatever it passed.
      latestMonthly.set(r.site_id, (latestMonthly.get(r.site_id) ?? 0) + ((r.price_per_gib_month_micros / 1e6) * r.bytes) / GIB);
    }
  }

  const finish = (t: DiskTotals) => {
    t.net_usd = t.earned_usd - t.spend_usd;
  };
  buckets.forEach(finish);
  finish(totals);

  const nameFromRows = new Map(rows.map((r) => [r.site_id, r.name]));
  const sites: SiteOut[] = [...mine.values()].map((s) => {
    const l = live.get(s.site_id);
    const price = l?.price_per_gib_month != null ? Number(l.price_per_gib_month) : NaN;
    const m = siteMoney.get(s.site_id) ?? { spend: 0, own: 0 };
    return {
      site_id: s.site_id,
      name: s.name || l?.name || nameFromRows.get(s.site_id) || s.site_id,
      url: s.url || `${gateway}/s/${s.site_id}/`,
      via: s.via,
      status: s.status ?? null,
      live: !!l,
      size_bytes: l ? l.size : null,
      copies: l ? l.copies : null,
      price_per_gib_month: Number.isFinite(price) ? price : null,
      holders: latestHolders.get(s.site_id)?.size ?? 0,
      monthly_usd: latestMonthly.get(s.site_id) ?? 0,
      spend_usd: m.spend,
      own_usd: m.own,
    };
  });
  sites.sort((a, b) => b.spend_usd - a.spend_usd || (b.size_bytes ?? 0) - (a.size_bytes ?? 0) || a.name.localeCompare(b.name));

  const nodes = [...nodeOut.values()].sort((a, b) => b.earned_usd - a.earned_usd || b.held_bytes - a.held_bytes || a.node.localeCompare(b.node));

  return { range, from, to: now, bucket_ms: bucketMs, totals, buckets, sites, nodes, latest_hour: latest };
}
