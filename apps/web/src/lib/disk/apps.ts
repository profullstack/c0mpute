// App hosting money: what each app used (CPU, RAM, disk, bandwidth) on the
// node that ran it, hour by hour, priced at that node's rates.
//
// The node running an app meters it and gossips one signed report per hour
// (node/crates/c0mpute-core/src/apps/usage.rs); every gateway serves what it
// heard at `GET /apps/v1/usage?from=&to=` for about 24 hours. This site files
// each report once, keyed (app, holder, hour). It is the holder's own account
// of the hour, attributable to it, not a proof; and nothing is paid from it
// yet, so the page says "metered".

import type { Row, Sql } from "../chat/store";

export const APPS_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS app_usage (
     app_id      TEXT NOT NULL,
     holder      TEXT NOT NULL,
     node        TEXT NOT NULL,
     hour        INTEGER NOT NULL,
     owner       TEXT,
     secs        INTEGER NOT NULL DEFAULT 0,
     cpu_s       REAL NOT NULL DEFAULT 0,
     ram_gib_h   REAL NOT NULL DEFAULT 0,
     disk_gib_h  REAL NOT NULL DEFAULT 0,
     rx_bytes    INTEGER NOT NULL DEFAULT 0,
     tx_bytes    INTEGER NOT NULL DEFAULT 0,
     rate_cpu_hour      INTEGER NOT NULL DEFAULT 0,
     rate_ram_gib_hour  INTEGER NOT NULL DEFAULT 0,
     rate_disk_gib_month INTEGER NOT NULL DEFAULT 0,
     rate_egress_gib    INTEGER NOT NULL DEFAULT 0,
     amount_micros INTEGER NOT NULL DEFAULT 0,
     PRIMARY KEY (app_id, holder, hour)
   )`,
  `CREATE INDEX IF NOT EXISTS idx_app_usage_node_hour ON app_usage(node, hour)`,
  `CREATE INDEX IF NOT EXISTS idx_app_usage_owner_hour ON app_usage(owner, hour)`,
];

export interface Rates {
  cpu_hour: number;
  ram_gib_hour: number;
  disk_gib_month: number;
  egress_gib: number;
}

/** One report as a gateway's `GET /apps/v1/usage` lists it (micro-USD rates). */
export interface UsageReport {
  app_id: string;
  owner: string;
  holder: string;
  /** Start of the hour, unix seconds. */
  hour: number;
  secs: number;
  cpu_s: number;
  ram_gib_h: number;
  disk_gib_h: number;
  rx_bytes: number;
  tx_bytes: number;
  rates: Rates;
}

export interface UsageRow extends Omit<UsageReport, "hour"> {
  node: string;
  /** Start of the hour, ms. */
  hour: number;
  amount_micros: number;
}

const GIB = 2 ** 30;
const APP_ID = /^[0-9a-f]{20}$/;
const OWNER = /^[0-9a-f]{64}$/;
const PEER = /^[1-9A-HJ-NP-Za-km-z]{8,128}$/;

/** Each part of an hour's bill, micro-USD, at the holder's rates. Same rule as the node. */
export function priceParts(r: Pick<UsageReport, "cpu_s" | "ram_gib_h" | "disk_gib_h" | "tx_bytes" | "rates">) {
  const cpu = (r.cpu_s / 3600) * r.rates.cpu_hour;
  const ram = r.ram_gib_h * r.rates.ram_gib_hour;
  const disk = (r.disk_gib_h / 720) * r.rates.disk_gib_month;
  const egress = (r.tx_bytes / GIB) * r.rates.egress_gib;
  return { cpu, ram, disk, egress, total: Math.floor(cpu + ram + disk + egress) };
}

/** A sane report from a gateway, or null. Anyone can run a gateway. */
export function cleanReport(x: unknown): UsageReport | null {
  if (!x || typeof x !== "object") return null;
  const r = x as Record<string, unknown>;
  const num = (v: unknown, max: number) => (typeof v === "number" && Number.isFinite(v) && v >= 0 && v <= max ? v : null);
  const rates = (r.rates ?? {}) as Record<string, unknown>;
  const rate = (v: unknown) => num(v, 1_000_000_000) ?? 0; // $1,000 a unit at most
  const app = typeof r.app_id === "string" && APP_ID.test(r.app_id) ? r.app_id : null;
  const holder = typeof r.holder === "string" && PEER.test(r.holder) ? r.holder : null;
  const hour = num(r.hour, 1e11);
  const cpu = num(r.cpu_s, 64 * 3600);
  const ram = num(r.ram_gib_h, 1024);
  const disk = num(r.disk_gib_h, 64 * 1024);
  if (!app || !holder || hour === null || hour % 3600 !== 0 || cpu === null || ram === null || disk === null) return null;
  return {
    app_id: app,
    owner: typeof r.owner === "string" && OWNER.test(r.owner) ? r.owner : "",
    holder,
    hour,
    secs: Math.min(num(r.secs, 3600) ?? 0, 3600),
    cpu_s: cpu,
    ram_gib_h: ram,
    disk_gib_h: disk,
    rx_bytes: Math.floor(num(r.rx_bytes, 2 ** 50) ?? 0),
    tx_bytes: Math.floor(num(r.tx_bytes, 2 ** 50) ?? 0),
    rates: {
      cpu_hour: rate(rates.cpu_hour),
      ram_gib_hour: rate(rates.ram_gib_hour),
      disk_gib_month: rate(rates.disk_gib_month),
      egress_gib: rate(rates.egress_gib),
    },
  };
}

export function createAppUsageStore(sql: Sql) {
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of APPS_SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  const toRow = (r: Row): UsageRow => ({
    app_id: String(r.app_id),
    holder: String(r.holder),
    node: String(r.node),
    hour: Number(r.hour),
    owner: String(r.owner ?? ""),
    secs: Number(r.secs) || 0,
    cpu_s: Number(r.cpu_s) || 0,
    ram_gib_h: Number(r.ram_gib_h) || 0,
    disk_gib_h: Number(r.disk_gib_h) || 0,
    rx_bytes: Number(r.rx_bytes) || 0,
    tx_bytes: Number(r.tx_bytes) || 0,
    rates: {
      cpu_hour: Number(r.rate_cpu_hour) || 0,
      ram_gib_hour: Number(r.rate_ram_gib_hour) || 0,
      disk_gib_month: Number(r.rate_disk_gib_month) || 0,
      egress_gib: Number(r.rate_egress_gib) || 0,
    },
    amount_micros: Number(r.amount_micros) || 0,
  });

  return {
    init,

    /** File reports; one already on file for that app, holder and hour is replaced. */
    async record(reports: UsageReport[]): Promise<void> {
      await init();
      for (let i = 0; i < reports.length; i += 40) {
        const chunk = reports.slice(i, i + 40);
        await sql.run(
          `INSERT OR REPLACE INTO app_usage (app_id, holder, node, hour, owner, secs, cpu_s, ram_gib_h, disk_gib_h, rx_bytes, tx_bytes,
             rate_cpu_hour, rate_ram_gib_hour, rate_disk_gib_month, rate_egress_gib, amount_micros)
           VALUES ${chunk.map(() => "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)").join(", ")}`,
          chunk.flatMap((r) => [
            r.app_id,
            r.holder,
            r.holder.slice(-8),
            r.hour * 1000,
            r.owner,
            r.secs,
            r.cpu_s,
            r.ram_gib_h,
            r.disk_gib_h,
            r.rx_bytes,
            r.tx_bytes,
            r.rates.cpu_hour,
            r.rates.ram_gib_hour,
            r.rates.disk_gib_month,
            r.rates.egress_gib,
            priceParts(r).total,
          ]),
        );
      }
    },

    /** Reports since `sinceMs` run by any of `nodes`, or for any of `apps` / apps of `owners`. */
    async usage(q: { sinceMs: number; nodes: string[]; apps: string[]; owners: string[] }): Promise<UsageRow[]> {
      await init();
      const ors: string[] = [];
      const params: unknown[] = [q.sinceMs];
      const inList = (col: string, vals: string[]) => {
        if (!vals.length) return;
        ors.push(`${col} IN (${vals.map(() => "?").join(", ")})`);
        params.push(...vals);
      };
      inList("node", q.nodes.slice(0, 1000));
      inList("app_id", q.apps.slice(0, 1000));
      inList("owner", q.owners.slice(0, 200));
      if (!ors.length) return [];
      return (await sql.all(`SELECT * FROM app_usage WHERE hour >= ? AND (${ors.join(" OR ")}) ORDER BY hour`, params)).map(toRow);
    },
  };
}

export type AppUsageStore = ReturnType<typeof createAppUsageStore>;
