// Server wiring for /dashboard/disk: your sites, your nodes, the recorded
// dues, and the live hosting picture from a gateway.
import "server-only";
import { pushStore } from "../chat/server";
import { operatorNodes } from "../earnings/server";
import { RANGES, type RangeKey } from "../earnings/report";
import { getNodesPayload, type NodeRow } from "../nodes";
import { buildDiskReport, type DiskReport, type LiveApp, type LiveSite, type MySite } from "./report";
import { appUsageStore, diskStore, gatewayUrls } from "./recorder";
import type { SiteClaim } from "./store";

export { diskStore };

let liveCache: { at: number; sites: LiveSite[] } | null = null;

/** Every hosted site a gateway knows, cached a minute. Empty when none answers. */
export async function liveSites(): Promise<LiveSite[]> {
  if (liveCache && Date.now() - liveCache.at < 60_000) return liveCache.sites;
  for (const base of gatewayUrls()) {
    try {
      const r = await fetch(`${base}/hosting/v1/sites`, { signal: AbortSignal.timeout(8_000), cache: "no-store" });
      if (!r.ok) continue;
      const d = (await r.json()) as { sites?: Record<string, unknown>[] };
      const sites: LiveSite[] = (d.sites ?? [])
        .filter((s) => typeof s.site_id === "string")
        .map((s) => ({
          site_id: String(s.site_id),
          name: typeof s.name === "string" ? s.name : "",
          owner: typeof s.owner === "string" ? s.owner : "",
          size: Number(s.size) || 0,
          copies: Number(s.copies) || 0,
          price_per_gib_month: typeof s.price_per_gib_month === "string" ? s.price_per_gib_month : null,
          preferred_hosts: Array.isArray(s.preferred_hosts) ? s.preferred_hosts.map(String) : [],
        }));
      liveCache = { at: Date.now(), sites };
      return sites;
    } catch {
      // next gateway
    }
  }
  return liveCache?.sites ?? [];
}

let appsCache: { at: number; apps: LiveApp[] } | null = null;

/** Every app a gateway knows, cached a minute. Empty when none answers. */
export async function liveApps(): Promise<LiveApp[]> {
  if (appsCache && Date.now() - appsCache.at < 60_000) return appsCache.apps;
  for (const base of gatewayUrls()) {
    try {
      const r = await fetch(`${base}/apps/v1/apps`, { signal: AbortSignal.timeout(8_000), cache: "no-store" });
      if (!r.ok) continue;
      const d = (await r.json()) as { apps?: Record<string, unknown>[] };
      const apps: LiveApp[] = (d.apps ?? [])
        .filter((a) => typeof a.app_id === "string")
        .map((a) => ({
          app_id: String(a.app_id),
          name: typeof a.name === "string" ? a.name : "",
          owner: typeof a.owner === "string" ? a.owner : "",
          healthy: a.healthy === true,
          holders: Array.isArray(a.holders) ? a.holders.map(String) : [],
          primary: typeof a.primary === "string" ? a.primary : null,
          replicas: Number(a.replicas) || 0,
        }));
      appsCache = { at: Date.now(), apps };
      return apps;
    } catch {
      // next gateway
    }
  }
  return appsCache?.apps ?? [];
}

export interface DiskDashboard extends DiskReport {
  generated_at: string;
  history_from: number | null;
  claims: SiteClaim[];
}

export async function diskDashboardFor(user: { id: string }, range: RangeKey, now = Date.now()): Promise<DiskDashboard> {
  const store = diskStore();
  const from = now - RANGES[range].ms;
  const [claims, pushes, nodes, board, live, apps, historyFrom] = await Promise.all([
    store.claims(user.id),
    pushStore()
      .pushes(user.id, "hosting")
      .catch(() => []),
    operatorNodes(user.id).catch(() => []),
    getNodesPayload().catch(() => null),
    liveSites(),
    liveApps(),
    store.firstHour().catch(() => null),
  ]);

  const sites: MySite[] = [];
  for (const p of pushes) {
    if (p.site_id) sites.push({ site_id: p.site_id, via: "push", name: p.name, status: p.status, url: p.url });
  }
  // A claimed 20-hex id is a site or an app: an app's id stays off the sites list.
  const liveSiteIds = new Set(live.map((s) => s.site_id));
  const liveAppIds = new Set(apps.map((a) => a.app_id));
  for (const c of claims) {
    if (c.kind === "site" && !(liveAppIds.has(c.value) && !liveSiteIds.has(c.value))) {
      sites.push({ site_id: c.value, via: "claim", name: c.label });
    }
  }
  const owners = claims.filter((c) => c.kind === "owner").map((c) => c.value);
  const siteIds = [...new Set([...sites.map((s) => s.site_id), ...live.filter((s) => owners.includes(s.owner)).map((s) => s.site_id)])];

  const online = new Map<string, NodeRow>((board?.nodes ?? []).map((n) => [n.id, n]));
  const claimedIds = claims.filter((c) => c.kind === "site").map((c) => c.value);
  const appIds = [...new Set([...claimedIds, ...apps.filter((a) => owners.includes(a.owner)).map((a) => a.app_id)])];
  const [rows, appRows] = await Promise.all([
    store.dues({ sinceMs: from - 3_600_000, nodes: nodes.map((n) => n.node), sites: siteIds, owners }),
    appUsageStore()
      .usage({ sinceMs: from - 3_600_000, nodes: nodes.map((n) => n.node), apps: appIds, owners })
      .catch(() => []),
  ]);
  const report = buildDiskReport({
    range,
    now,
    rows,
    nodes: nodes.map((c) => {
      const b = online.get(c.node);
      return { node: c.node, label: c.label ?? b?.name ?? null, online: !!b, offer: b?.storage ?? null };
    }),
    sites,
    owners,
    live,
    gateway: gatewayUrls()[0],
    appRows,
    liveApps: apps,
    appIds: claimedIds,
  });
  return { ...report, generated_at: new Date(now).toISOString(), history_from: historyFrom, claims };
}
