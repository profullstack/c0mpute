// Durable disk (hosting) money: what every paid site's payer owes each node
// that holds it, hour by hour, for /dashboard/disk.
//
// A paid site's owner pays its holders peer to peer, from its own node, by
// one rule (node/crates/c0mpute-core/src/hosting/settle.rs):
//
//   owed = price/GiB-month × GiB held × (window / 30 days) × passed / challenged
//
// Every input (signed site records, signed availability receipts, who holds
// what) is gossiped, so any gateway can price every site:
// `GET /hosting/v1/dues?from=&to=`. Receipts are kept for a few hours only,
// so this site copies each finished hour into SQLite Cloud, one row per
// (site, holder, hour). Two gateways that both priced an hour file it once;
// the row that saw more challenges wins.
//
// What this is NOT: proof of payment. The payer's node pays from its own
// CoinPay wallet once a settlement period ([payments] on), or with payments
// off only logs what it owes. So the page says "accrued".
//
// Who is whose: a holder is a node (its peer id), which an operator claims on
// /dashboard/nodes. A site is a person's when they pushed it from c0mpute
// chat (chat_pushes.site_id), or claimed its id or its owner key here.

import type { Row, Sql } from "../chat/store";

export const DISK_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS disk_dues (
     site_id     TEXT NOT NULL,
     holder      TEXT NOT NULL,
     node        TEXT NOT NULL,
     hour        INTEGER NOT NULL,
     name        TEXT,
     owner       TEXT,
     payer       TEXT,
     bytes       INTEGER NOT NULL DEFAULT 0,
     price_micros INTEGER NOT NULL DEFAULT 0,
     amount_micros INTEGER NOT NULL DEFAULT 0,
     passed      INTEGER NOT NULL DEFAULT 0,
     challenged  INTEGER NOT NULL DEFAULT 0,
     payout      INTEGER NOT NULL DEFAULT 0,
     PRIMARY KEY (site_id, holder, hour)
   )`,
  `CREATE INDEX IF NOT EXISTS idx_disk_dues_node_hour ON disk_dues(node, hour)`,
  `CREATE INDEX IF NOT EXISTS idx_disk_dues_hour ON disk_dues(hour)`,
  `CREATE INDEX IF NOT EXISTS idx_disk_dues_owner_hour ON disk_dues(owner, hour)`,
  `CREATE TABLE IF NOT EXISTS disk_dues_cursor (
     source TEXT PRIMARY KEY,
     until_s INTEGER NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS disk_site_claims (
     user_id    TEXT NOT NULL,
     kind       TEXT NOT NULL,
     value      TEXT NOT NULL,
     label      TEXT,
     created_at TEXT NOT NULL,
     PRIMARY KEY (user_id, kind, value)
   )`,
  `CREATE INDEX IF NOT EXISTS idx_disk_site_claims_value ON disk_site_claims(kind, value)`,
];

/** One due as a gateway's `GET /hosting/v1/dues` lists it. */
export interface GatewayDue {
  site_id: string;
  name: string;
  owner: string;
  payer: string;
  holder: string;
  bytes: number;
  price_per_gib_month_micros: number;
  amount_micros: number;
  passed: number;
  challenged: number;
  payout: boolean;
}

/** One stored row: a due for one site, holder and hour. */
export interface DueRow extends GatewayDue {
  /** The board's id for the holder: the last 8 characters of its peer id. */
  node: string;
  /** Start of the hour, ms. */
  hour: number;
}

/** What a person claims as theirs: a site id, or every site of an owner key. */
export type SiteClaimKind = "site" | "owner";
export interface SiteClaim {
  kind: SiteClaimKind;
  value: string;
  label: string | null;
  created_at: string;
}

export const MAX_SITE_CLAIMS = 500;

const SITE_ID = /^[0-9a-f]{20}$/;
const OWNER = /^[0-9a-f]{64}$/;
const PEER = /^[1-9A-HJ-NP-Za-km-z]{8,128}$/;
const MAX_MICROS = 1_000_000_000_000; // $1M: anything above is a broken gateway.
const MAX_BYTES = 64 * 2 ** 40; // 64 TiB, far above the 4 TiB site cap.

/** A site id or owner key as a person typed it, or null. */
export function parseSiteClaim(raw: unknown): { kind: SiteClaimKind; value: string } | null {
  if (typeof raw !== "string") return null;
  let s = raw.trim().toLowerCase();
  // A pasted site URL: https://gateway.c0mpute.com/s/<id>/...
  const m = s.match(/\/[sp]\/([0-9a-f]{20})(?:[/?#]|$)/);
  if (m) s = m[1];
  if (SITE_ID.test(s)) return { kind: "site", value: s };
  if (OWNER.test(s)) return { kind: "owner", value: s };
  return null;
}

/** A sane due from a gateway, or null. Anyone can run a gateway. */
export function cleanDue(d: unknown): GatewayDue | null {
  if (!d || typeof d !== "object") return null;
  const r = d as Record<string, unknown>;
  const int = (v: unknown, max: number) =>
    typeof v === "number" && Number.isFinite(v) && v >= 0 ? Math.min(Math.floor(v), max) : null;
  const site = typeof r.site_id === "string" && SITE_ID.test(r.site_id) ? r.site_id : null;
  const holder = typeof r.holder === "string" && PEER.test(r.holder) ? r.holder : null;
  const hex = (v: unknown) => (typeof v === "string" && OWNER.test(v) ? v : "");
  const bytes = int(r.bytes, MAX_BYTES);
  const amount = int(r.amount_micros, MAX_MICROS);
  if (!site || !holder || bytes === null || amount === null) return null;
  const challenged = int(r.challenged, 1_000_000) ?? 0;
  return {
    site_id: site,
    name: typeof r.name === "string" ? r.name.slice(0, 80) : "",
    owner: hex(r.owner),
    payer: hex(r.payer),
    holder,
    bytes,
    price_per_gib_month_micros: int(r.price_per_gib_month_micros, MAX_MICROS) ?? 0,
    amount_micros: amount,
    passed: Math.min(int(r.passed, 1_000_000) ?? 0, challenged),
    challenged,
    payout: r.payout === true,
  };
}

const iso = () => new Date().toISOString();

export function createDiskStore(sql: Sql) {
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of DISK_SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  const toRow = (r: Row): DueRow => ({
    site_id: String(r.site_id),
    holder: String(r.holder),
    node: String(r.node),
    hour: Number(r.hour),
    name: String(r.name ?? ""),
    owner: String(r.owner ?? ""),
    payer: String(r.payer ?? ""),
    bytes: Number(r.bytes) || 0,
    price_per_gib_month_micros: Number(r.price_micros) || 0,
    amount_micros: Number(r.amount_micros) || 0,
    passed: Number(r.passed) || 0,
    challenged: Number(r.challenged) || 0,
    payout: Number(r.payout) === 1,
  });

  return {
    init,

    /**
     * File one hour's dues. An hour already on file is replaced only by an
     * answer that saw more challenges (a gateway with more receipts).
     */
    async recordHour(hourMs: number, dues: GatewayDue[]): Promise<void> {
      await init();
      // 13 params a row; stay far under SQLite's bound-parameter limit.
      for (let i = 0; i < dues.length; i += 50) {
        const chunk = dues.slice(i, i + 50);
        await sql.run(
          `INSERT INTO disk_dues (site_id, holder, node, hour, name, owner, payer, bytes, price_micros, amount_micros, passed, challenged, payout)
           VALUES ${chunk.map(() => "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)").join(", ")}
           ON CONFLICT (site_id, holder, hour) DO UPDATE SET
             name = excluded.name, owner = excluded.owner, payer = excluded.payer, bytes = excluded.bytes,
             price_micros = excluded.price_micros, amount_micros = excluded.amount_micros,
             passed = excluded.passed, challenged = excluded.challenged, payout = excluded.payout
           WHERE excluded.challenged > disk_dues.challenged`,
          chunk.flatMap((d) => [
            d.site_id,
            d.holder,
            d.holder.slice(-8),
            hourMs,
            d.name,
            d.owner,
            d.payer,
            d.bytes,
            d.price_per_gib_month_micros,
            d.amount_micros,
            d.passed,
            d.challenged,
            d.payout ? 1 : 0,
          ]),
        );
      }
    },

    async cursor(source: string): Promise<number | null> {
      await init();
      const r = (await sql.all(`SELECT until_s FROM disk_dues_cursor WHERE source = ?`, [source]))[0];
      return r ? Number(r.until_s) || null : null;
    },

    async setCursor(source: string, untilS: number): Promise<void> {
      await init();
      await sql.run(`INSERT OR REPLACE INTO disk_dues_cursor (source, until_s) VALUES (?, ?)`, [source, untilS]);
    },

    /**
     * Dues since `sinceMs` held by any of `nodes` (board ids), or for any of
     * `sites` / sites of any of `owners`. Both sides in one read: a person
     * who both runs nodes and owns sites sees money they paid themselves.
     */
    async dues(q: { sinceMs: number; nodes: string[]; sites: string[]; owners: string[] }): Promise<DueRow[]> {
      await init();
      const ors: string[] = [];
      const params: unknown[] = [q.sinceMs];
      const inList = (col: string, vals: string[]) => {
        if (!vals.length) return;
        ors.push(`${col} IN (${vals.map(() => "?").join(", ")})`);
        params.push(...vals);
      };
      inList("node", q.nodes.slice(0, 1000));
      inList("site_id", q.sites.slice(0, 1000));
      inList("owner", q.owners.slice(0, 200));
      if (!ors.length) return [];
      const rows = await sql.all(`SELECT * FROM disk_dues WHERE hour >= ? AND (${ors.join(" OR ")}) ORDER BY hour`, params);
      return rows.map(toRow);
    },

    /** Network totals since `sinceMs`, per hour: the public picture. */
    async network(sinceMs: number): Promise<{ hour: number; amount_micros: number; bytes: number; sites: number; holders: number }[]> {
      await init();
      const rows = await sql.all(
        `SELECT hour, SUM(amount_micros) AS amount, SUM(bytes) AS bytes, COUNT(DISTINCT site_id) AS sites, COUNT(DISTINCT holder) AS holders
           FROM disk_dues WHERE hour >= ? GROUP BY hour ORDER BY hour`,
        [sinceMs],
      );
      return rows.map((r) => ({
        hour: Number(r.hour),
        amount_micros: Number(r.amount) || 0,
        bytes: Number(r.bytes) || 0,
        sites: Number(r.sites) || 0,
        holders: Number(r.holders) || 0,
      }));
    },

    async firstHour(): Promise<number | null> {
      await init();
      const r = (await sql.all(`SELECT MIN(hour) AS h FROM disk_dues`))[0];
      return r?.h == null ? null : Number(r.h);
    },

    // ── site claims ────────────────────────────────────────────────────────
    async claims(userId: string): Promise<SiteClaim[]> {
      await init();
      return (await sql.all(`SELECT * FROM disk_site_claims WHERE user_id = ? ORDER BY created_at`, [userId])).map((r) => ({
        kind: String(r.kind) as SiteClaimKind,
        value: String(r.value),
        label: (r.label as string) ?? null,
        created_at: String(r.created_at),
      }));
    },

    async claim(userId: string, c: { kind: SiteClaimKind; value: string; label?: string | null }): Promise<void> {
      await init();
      const have = (await sql.all(`SELECT 1 FROM disk_site_claims WHERE user_id = ? AND kind = ? AND value = ?`, [userId, c.kind, c.value])).length > 0;
      if (have) {
        if (c.label !== undefined) {
          await sql.run(`UPDATE disk_site_claims SET label = ? WHERE user_id = ? AND kind = ? AND value = ?`, [c.label ?? null, userId, c.kind, c.value]);
        }
        return;
      }
      const n = (await sql.all(`SELECT COUNT(*) AS n FROM disk_site_claims WHERE user_id = ?`, [userId]))[0];
      if (Number(n?.n ?? 0) >= MAX_SITE_CLAIMS) throw new Error(`at most ${MAX_SITE_CLAIMS} sites per account`);
      await sql.run(`INSERT INTO disk_site_claims (user_id, kind, value, label, created_at) VALUES (?, ?, ?, ?, ?)`, [
        userId,
        c.kind,
        c.value,
        c.label ?? null,
        iso(),
      ]);
    },

    async unclaim(userId: string, kind: SiteClaimKind, value: string): Promise<boolean> {
      await init();
      const found = (await sql.all(`SELECT 1 FROM disk_site_claims WHERE user_id = ? AND kind = ? AND value = ?`, [userId, kind, value])).length > 0;
      if (found) await sql.run(`DELETE FROM disk_site_claims WHERE user_id = ? AND kind = ? AND value = ?`, [userId, kind, value]);
      return found;
    },
  };
}

export type DiskStore = ReturnType<typeof createDiskStore>;
