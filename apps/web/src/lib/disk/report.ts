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
import { priceParts, type UsageRow } from "./apps";

const HOUR = 3_600_000;
/** Lowest price any site settles at, $/GiB-month (settle.rs FLOOR_PER_GIB_MONTH). */
export const FLOOR_PER_GIB_MONTH = 0.004;
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
  /** $/GiB-month it settles at: its price, never below the floor. */
  price_per_gib_month: number;
  /** The floor applies (published without a price, or below it). */
  price_floor: boolean;
  /** Nodes paid for holding it in the newest recorded hour. */
  holders: number;
  /** What it costs a month at the newest hour's rate (accrued). */
  monthly_usd: number;
  spend_usd: number;
  own_usd: number;
}

/** An app as a gateway's `/apps/v1/apps` lists it. */
export interface LiveApp {
  app_id: string;
  name: string;
  owner: string;
  healthy: boolean;
  holders: string[];
  primary: string | null;
  replicas: number;
}

export interface AppOut {
  app_id: string;
  name: string;
  url: string;
  via: "claim" | "owner";
  live: boolean;
  healthy: boolean | null;
  holders: number;
  /** Metered in the window. */
  hours: number;
  cpu_hours: number;
  ram_gib_hours: number;
  /** Average disk held while running, GiB. */
  disk_gib: number;
  egress_bytes: number;
  ingress_bytes: number;
  /** What each part cost in the window. */
  cpu_usd: number;
  ram_usd: number;
  disk_usd: number;
  egress_usd: number;
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
  /** Running others' apps: earned, CPU-hours served, apps run in the window. */
  app_earned_usd: number;
  app_own_usd: number;
  app_cpu_hours: number;
  apps: number;
}

export interface DiskReport {
  range: RangeKey;
  from: number;
  to: number;
  bucket_ms: number;
  totals: DiskTotals;
  buckets: DiskBucket[];
  sites: SiteOut[];
  apps: AppOut[];
  nodes: NodeDiskOut[];
  /** The totals split: static storage vs apps. `totals` is their sum. */
  by_kind: { storage: DiskTotals; apps: DiskTotals };
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
  appRows?: UsageRow[];
  liveApps?: LiveApp[];
  /** App ids this person claimed (a claimed 20-hex id may be a site or an app). */
  appIds?: string[];
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
      {
        node: x.node,
        label: x.label,
        online: x.online,
        held_bytes: 0,
        sites: 0,
        earned_usd: 0,
        own_usd: 0,
        passed: 0,
        challenged: 0,
        offer: x.offer ?? null,
        app_earned_usd: 0,
        app_own_usd: 0,
        app_cpu_hours: 0,
        apps: 0,
      },
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

  const storage = { ...totals };

  // ── apps ──
  const appsTotals = zero();
  const liveApps = new Map((input.liveApps ?? []).map((a) => [a.app_id, a]));
  const myApps = new Map<string, AppOut["via"]>();
  for (const id of input.appIds ?? []) myApps.set(id, "claim");
  for (const a of input.liveApps ?? []) if (owners.has(a.owner) && !myApps.has(a.app_id)) myApps.set(a.app_id, "owner");
  const appRows = (input.appRows ?? []).filter((r) => r.hour + HOUR > from && r.hour < now);
  for (const r of appRows) if (owners.has(r.owner) && !myApps.has(r.app_id)) myApps.set(r.app_id, "owner");
  const appOut = new Map<string, AppOut>();
  for (const [id, via] of myApps) {
    const l = liveApps.get(id);
    // A claimed id that is not an app (a site id) stays off the apps list
    // unless usage names it.
    if (via === "claim" && !l && !appRows.some((r) => r.app_id === id)) continue;
    appOut.set(id, {
      app_id: id,
      name: l?.name || id,
      url: `${gateway}/a/${id}/`,
      via,
      live: !!l,
      healthy: l ? l.healthy : null,
      holders: l?.holders.length ?? 0,
      hours: 0,
      cpu_hours: 0,
      ram_gib_hours: 0,
      disk_gib: 0,
      egress_bytes: 0,
      ingress_bytes: 0,
      cpu_usd: 0,
      ram_usd: 0,
      disk_usd: 0,
      egress_usd: 0,
      spend_usd: 0,
      own_usd: 0,
    });
  }
  const nodeApps = new Map<string, Set<string>>();
  const diskGibH = new Map<string, number>();
  for (const r of appRows) {
    const nodeMine = myNodes.has(r.node);
    const app = appOut.get(r.app_id);
    if (!nodeMine && !app) continue;
    const kind: keyof DiskTotals = nodeMine && app ? "own_usd" : nodeMine ? "earned_usd" : "spend_usd";
    const parts = priceParts(r);
    const usd = parts.total / 1e6;
    // Only the part of the hour inside the window.
    let inWindow = 0;
    for (const b of buckets) {
      const lo = Math.max(r.hour, b.t, from);
      const hi = Math.min(r.hour + HOUR, b.t + bucketMs, now);
      if (hi <= lo) continue;
      const f = (hi - lo) / HOUR;
      inWindow += f;
      b[kind] += usd * f;
      appsTotals[kind] += usd * f;
    }
    if (nodeMine) {
      const nd = nodeOut.get(r.node)!;
      if (kind === "own_usd") nd.app_own_usd += usd * inWindow;
      else nd.app_earned_usd += usd * inWindow;
      nd.app_cpu_hours += (r.cpu_s / 3600) * inWindow;
      const set = nodeApps.get(r.node) ?? new Set<string>();
      set.add(r.app_id);
      nodeApps.set(r.node, set);
    }
    if (app) {
      app.hours += (r.secs / 3600) * inWindow;
      app.cpu_hours += (r.cpu_s / 3600) * inWindow;
      app.ram_gib_hours += r.ram_gib_h * inWindow;
      diskGibH.set(r.app_id, (diskGibH.get(r.app_id) ?? 0) + r.disk_gib_h * inWindow);
      app.egress_bytes += r.tx_bytes * inWindow;
      app.ingress_bytes += r.rx_bytes * inWindow;
      app.cpu_usd += (parts.cpu / 1e6) * inWindow;
      app.ram_usd += (parts.ram / 1e6) * inWindow;
      app.disk_usd += (parts.disk / 1e6) * inWindow;
      app.egress_usd += (parts.egress / 1e6) * inWindow;
      if (kind === "own_usd") app.own_usd += usd * inWindow;
      else app.spend_usd += usd * inWindow;
    }
  }
  for (const [node, set] of nodeApps) nodeOut.get(node)!.apps = set.size;
  for (const a of appOut.values()) {
    a.disk_gib = a.hours > 0 ? (diskGibH.get(a.app_id) ?? 0) / a.hours : 0;
    a.egress_bytes = Math.round(a.egress_bytes);
    a.ingress_bytes = Math.round(a.ingress_bytes);
  }
  for (const k of ["earned_usd", "spend_usd", "own_usd"] as const) totals[k] += appsTotals[k];

  const finish = (t: DiskTotals) => {
    t.net_usd = t.earned_usd - t.spend_usd;
  };
  buckets.forEach(finish);
  finish(totals);
  finish(storage);
  finish(appsTotals);

  const nameFromRows = new Map(rows.map((r) => [r.site_id, r.name]));
  const sites: SiteOut[] = [...mine.values()].map((s) => {
    const l = live.get(s.site_id);
    const asked = l?.price_per_gib_month != null ? Number(l.price_per_gib_month) : NaN;
    const floored = !Number.isFinite(asked) || asked < FLOOR_PER_GIB_MONTH;
    const price = floored ? FLOOR_PER_GIB_MONTH : asked;
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
      price_per_gib_month: price,
      price_floor: floored,
      holders: latestHolders.get(s.site_id)?.size ?? 0,
      // Until an hour of dues is on file: its size x copies at its price.
      monthly_usd: latestMonthly.get(s.site_id) ?? (l ? ((l.size * Math.max(1, l.copies)) / GIB) * price : 0),
      spend_usd: m.spend,
      own_usd: m.own,
    };
  });
  sites.sort((a, b) => b.spend_usd - a.spend_usd || (b.size_bytes ?? 0) - (a.size_bytes ?? 0) || a.name.localeCompare(b.name));

  const nodes = [...nodeOut.values()].sort(
    (a, b) => b.earned_usd + b.app_earned_usd - (a.earned_usd + a.app_earned_usd) || b.held_bytes - a.held_bytes || a.node.localeCompare(b.node),
  );
  const apps = [...appOut.values()].sort((a, b) => b.spend_usd - a.spend_usd || b.cpu_hours - a.cpu_hours || a.name.localeCompare(b.name));

  return {
    range,
    from,
    to: now,
    bucket_ms: bucketMs,
    totals,
    by_kind: { storage, apps: appsTotals },
    buckets,
    sites,
    apps,
    nodes,
    latest_hour: latest,
  };
}
