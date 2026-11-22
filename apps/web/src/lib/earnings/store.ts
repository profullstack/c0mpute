// Durable node earnings history for the operator dashboard (/dashboard/nodes).
//
// The status aggregators keep each node's completed jobs for 24h in memory
// only, and a restart (every web deploy restarts the co-located one) starts
// the window over. So this site copies every credited job out of every
// aggregator (`GET /jobs?since=`, one row per job id) into its own SQLite
// Cloud database, where it stays. Two aggregators that both saw a job file it
// once: the job id is the primary key.
//
// Operators say which nodes are theirs (operator_nodes) and what a node costs
// them to run; the dashboard prices spend from that ($0 when unset).

import type { Row, Sql } from "../chat/store";

export const EARNINGS_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS node_jobs (
     job_id       TEXT PRIMARY KEY,
     node         TEXT NOT NULL,
     type         TEXT NOT NULL,
     completed_at INTEGER NOT NULL,
     busy_s       REAL NOT NULL DEFAULT 0,
     paid_usd     REAL NOT NULL DEFAULT 0
   )`,
  `CREATE INDEX IF NOT EXISTS idx_node_jobs_node_at ON node_jobs(node, completed_at)`,
  `CREATE INDEX IF NOT EXISTS idx_node_jobs_at ON node_jobs(completed_at)`,
  `CREATE TABLE IF NOT EXISTS node_jobs_cursor (
     source  TEXT PRIMARY KEY,
     seen_at INTEGER NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS operator_nodes (
     user_id           TEXT NOT NULL,
     node              TEXT NOT NULL,
     peer_id           TEXT,
     label             TEXT,
     class             TEXT,
     cost_usd_per_hour REAL,
     via               TEXT NOT NULL,
     created_at        TEXT NOT NULL,
     PRIMARY KEY (user_id, node)
   )`,
  `CREATE INDEX IF NOT EXISTS idx_operator_nodes_node ON operator_nodes(node)`,
];

/** One credited job as an aggregator's `GET /jobs` lists it. */
export interface AggregatorJob {
  job_id: string;
  node: string;
  type: string;
  completed_at_ms: number;
  seen_at_ms: number;
  busy_s: number;
  paid_usd: number;
}

/** How a claim got here: from the node's own API key, the browser, or a hosting link. */
export type ClaimVia = "key" | "web" | "hosting";

export interface OperatorNode {
  node: string;
  peer_id: string | null;
  label: string | null;
  class: string | null;
  cost_usd_per_hour: number | null;
  via: ClaimVia;
  created_at: string;
}

/** Sums for one node, workload type and time bucket. */
export interface JobBucketRow {
  node: string;
  type: string;
  bucket: number;
  jobs: number;
  busy_s: number;
  paid_usd: number;
}

/** Claims per account: a fleet, not the whole network. */
export const MAX_CLAIMS = 500;
/** Highest running cost we accept for one node ($/hour): an H100 rents for a few. */
export const MAX_COST_PER_HOUR = 1_000;

const BASE58 = /^[1-9A-HJ-NP-Za-km-z]+$/;

/**
 * The board's row id for what an operator typed: a full peer id
 * (`c0mpute status --json` -> peer_id) or the 8-character id /nodes shows.
 * The board keys rows on the last 8 characters of the peer id.
 */
export function nodeKey(raw: unknown): { node: string; peer_id: string | null } | null {
  if (typeof raw !== "string") return null;
  const s = raw.trim();
  if (s.length < 8 || s.length > 128 || !BASE58.test(s)) return null;
  return { node: s.slice(-8), peer_id: s.length > 8 ? s : null };
}

/** A sane job row from an aggregator, or null. Anyone can run an aggregator. */
export function cleanJob(j: unknown): AggregatorJob | null {
  if (!j || typeof j !== "object") return null;
  const r = j as Record<string, unknown>;
  const num = (v: unknown, max: number) => (typeof v === "number" && Number.isFinite(v) && v >= 0 ? Math.min(v, max) : null);
  const id = typeof r.job_id === "string" && r.job_id.length > 0 && r.job_id.length <= 128 ? r.job_id : null;
  const node = typeof r.node === "string" && /^[1-9A-HJ-NP-Za-km-z]{1,16}$/.test(r.node) ? r.node : null;
  const type = typeof r.type === "string" && /^[a-z0-9._:-]{1,64}$/i.test(r.type) ? r.type : "unknown";
  const at = num(r.completed_at_ms, Date.now() + 60_000);
  const seen = num(r.seen_at_ms, Number.MAX_SAFE_INTEGER);
  if (!id || !node || at === null || seen === null) return null;
  return {
    job_id: id,
    node,
    type,
    completed_at_ms: Math.floor(at),
    seen_at_ms: Math.floor(seen),
    // A day of run time per job at most, and the aggregator's own price cap.
    busy_s: num(r.busy_s, 86_400) ?? 0,
    paid_usd: num(r.paid_usd, 1_000) ?? 0,
  };
}

const now = () => new Date().toISOString();

export function createEarningsStore(sql: Sql) {
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of EARNINGS_SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  const toClaim = (r: Row): OperatorNode => ({
    node: String(r.node),
    peer_id: (r.peer_id as string) ?? null,
    label: (r.label as string) ?? null,
    class: (r.class as string) ?? null,
    cost_usd_per_hour: r.cost_usd_per_hour == null ? null : Number(r.cost_usd_per_hour),
    via: String(r.via) as ClaimVia,
    created_at: String(r.created_at),
  });

  return {
    init,

    // ── job history ────────────────────────────────────────────────────────
    /** File jobs; ones already filed (by id) are left as they are. */
    async recordJobs(jobs: AggregatorJob[]): Promise<void> {
      await init();
      // 6 params a row; stay far under SQLite's bound-parameter limit.
      for (let i = 0; i < jobs.length; i += 100) {
        const chunk = jobs.slice(i, i + 100);
        await sql.run(
          `INSERT OR IGNORE INTO node_jobs (job_id, node, type, completed_at, busy_s, paid_usd) VALUES ${chunk
            .map(() => "(?, ?, ?, ?, ?, ?)")
            .join(", ")}`,
          chunk.flatMap((j) => [j.job_id, j.node, j.type, j.completed_at_ms, j.busy_s, j.paid_usd]),
        );
      }
    },

    async cursor(source: string): Promise<number> {
      await init();
      const r = (await sql.all(`SELECT seen_at FROM node_jobs_cursor WHERE source = ?`, [source]))[0];
      return r ? Number(r.seen_at) || 0 : 0;
    },

    async setCursor(source: string, seenAt: number): Promise<void> {
      await init();
      await sql.run(`INSERT OR REPLACE INTO node_jobs_cursor (source, seen_at) VALUES (?, ?)`, [source, seenAt]);
    },

    /**
     * Jobs, run time and takings per node, type and bucket since `sinceMs`.
     * `nodes` null means every node (the network view).
     */
    async buckets(nodes: string[] | null, sinceMs: number, bucketMs: number): Promise<JobBucketRow[]> {
      await init();
      if (nodes && nodes.length === 0) return [];
      const where = nodes ? `AND node IN (${nodes.map(() => "?").join(", ")})` : "";
      const rows = await sql.all(
        `SELECT node, type, (completed_at / ?) AS bucket, COUNT(*) AS jobs, SUM(busy_s) AS busy_s, SUM(paid_usd) AS paid_usd
           FROM node_jobs WHERE completed_at >= ? ${where}
          GROUP BY node, type, bucket`,
        [bucketMs, sinceMs, ...(nodes ?? [])],
      );
      return rows.map((r) => ({
        node: String(r.node),
        type: String(r.type),
        bucket: Number(r.bucket),
        jobs: Number(r.jobs) || 0,
        busy_s: Number(r.busy_s) || 0,
        paid_usd: Number(r.paid_usd) || 0,
      }));
    },

    /**
     * Jobs and buyer-agreed takings per node since `sinceMs` (0 = all history),
     * for the network-wide earnings on /status. Callers aggregate; no node is shown.
     */
    async nodeTotals(sinceMs: number): Promise<{ node: string; jobs: number; paid_usd: number }[]> {
      await init();
      const rows = await sql.all(
        `SELECT node, COUNT(*) AS jobs, SUM(paid_usd) AS paid_usd FROM node_jobs WHERE completed_at >= ? GROUP BY node`,
        [sinceMs],
      );
      return rows.map((r) => ({ node: String(r.node), jobs: Number(r.jobs) || 0, paid_usd: Number(r.paid_usd) || 0 }));
    },

    /** Every node's jobs, run time and agreed takings per workload type since `sinceMs` (the network view). */
    async networkSince(sinceMs: number): Promise<Omit<JobBucketRow, "bucket">[]> {
      await init();
      const rows = await sql.all(
        `SELECT node, type, COUNT(*) AS jobs, SUM(busy_s) AS busy_s, SUM(paid_usd) AS paid_usd
           FROM node_jobs WHERE completed_at >= ? GROUP BY node, type`,
        [sinceMs],
      );
      return rows.map((r) => ({
        node: String(r.node),
        type: String(r.type),
        jobs: Number(r.jobs) || 0,
        busy_s: Number(r.busy_s) || 0,
        paid_usd: Number(r.paid_usd) || 0,
      }));
    },

    /** The hardware class each claimed node last reported, across all accounts. */
    async claimedClasses(): Promise<Map<string, string>> {
      await init();
      const rows = await sql.all(`SELECT node, class FROM operator_nodes WHERE class IS NOT NULL`);
      return new Map(rows.map((r) => [String(r.node), String(r.class)]));
    },

    /** The first job on file, so the page can say how far back history goes. */
    async firstJobAt(): Promise<number | null> {
      await init();
      const r = (await sql.all(`SELECT MIN(completed_at) AS at FROM node_jobs`))[0];
      return r?.at == null ? null : Number(r.at);
    },

    // ── claims ─────────────────────────────────────────────────────────────
    async claims(userId: string): Promise<OperatorNode[]> {
      await init();
      return (await sql.all(`SELECT * FROM operator_nodes WHERE user_id = ? ORDER BY created_at, node`, [userId])).map(toClaim);
    },

    /** Add a node to the account (or refresh how it was claimed). */
    async claim(
      userId: string,
      c: { node: string; peer_id?: string | null; label?: string | null; class?: string | null; via: ClaimVia },
    ): Promise<OperatorNode> {
      await init();
      const existing = (await sql.all(`SELECT * FROM operator_nodes WHERE user_id = ? AND node = ?`, [userId, c.node]))[0];
      if (existing) {
        // A claim from the node itself outranks one typed into a browser.
        const via = existing.via === "key" || c.via === "key" ? "key" : (existing.via as ClaimVia);
        await sql.run(
          `UPDATE operator_nodes SET peer_id = COALESCE(?, peer_id), label = COALESCE(?, label), class = COALESCE(?, class), via = ? WHERE user_id = ? AND node = ?`,
          [c.peer_id ?? null, c.label ?? null, c.class ?? null, via, userId, c.node],
        );
      } else {
        const n = (await sql.all(`SELECT COUNT(*) AS n FROM operator_nodes WHERE user_id = ?`, [userId]))[0];
        if (Number(n?.n ?? 0) >= MAX_CLAIMS) throw new Error(`at most ${MAX_CLAIMS} nodes per account`);
        await sql.run(
          `INSERT INTO operator_nodes (user_id, node, peer_id, label, class, cost_usd_per_hour, via, created_at) VALUES (?, ?, ?, ?, ?, NULL, ?, ?)`,
          [userId, c.node, c.peer_id ?? null, c.label ?? null, c.class ?? null, c.via, now()],
        );
      }
      const row = (await sql.all(`SELECT * FROM operator_nodes WHERE user_id = ? AND node = ?`, [userId, c.node]))[0];
      return toClaim(row);
    },

    /** Set (or with null, clear) what a node costs to run, and its label. */
    async update(
      userId: string,
      node: string,
      patch: { cost_usd_per_hour?: number | null; label?: string | null; class?: string | null },
    ): Promise<boolean> {
      await init();
      const sets: string[] = [];
      const params: unknown[] = [];
      for (const k of ["cost_usd_per_hour", "label", "class"] as const) {
        if (k in patch) {
          sets.push(`${k} = ?`);
          params.push(patch[k] ?? null);
        }
      }
      const found = (await sql.all(`SELECT node FROM operator_nodes WHERE user_id = ? AND node = ?`, [userId, node])).length > 0;
      if (!found) return false;
      if (sets.length) await sql.run(`UPDATE operator_nodes SET ${sets.join(", ")} WHERE user_id = ? AND node = ?`, [...params, userId, node]);
      return true;
    },

    async unclaim(userId: string, node: string): Promise<boolean> {
      await init();
      const found = (await sql.all(`SELECT node FROM operator_nodes WHERE user_id = ? AND node = ?`, [userId, node])).length > 0;
      if (found) await sql.run(`DELETE FROM operator_nodes WHERE user_id = ? AND node = ?`, [userId, node]);
      return found;
    },
  };
}

export type EarningsStore = ReturnType<typeof createEarningsStore>;
