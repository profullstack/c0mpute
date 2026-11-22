// Leaderboard storage in the site's SQLite Cloud database:
//  - usage_daily: calls per account, UTC day, surface (chat / api / mcp) and
//    item (the model asked for, or the MCP tool). Counters only: never a
//    prompt, a message, an argument or a result. History starts the day this
//    shipped (chat runs themselves are deleted an hour after they finish).
//  - leaderboard_optout: accounts that asked to be left off /leaderboard.
// Written against the two-method Sql interface so tests run on bun:sqlite.
import type { Sql } from "../chat/store";
import { cleanLabel } from "./anon";

export const LEADERBOARD_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS usage_daily (
     day     TEXT NOT NULL,
     user_id TEXT NOT NULL,
     surface TEXT NOT NULL,
     item    TEXT NOT NULL,
     calls   INTEGER NOT NULL DEFAULT 0,
     PRIMARY KEY (day, user_id, surface, item)
   )`,
  `CREATE INDEX IF NOT EXISTS idx_usage_daily_day ON usage_daily(day)`,
  `CREATE TABLE IF NOT EXISTS leaderboard_optout (
     user_id    TEXT PRIMARY KEY,
     created_at TEXT NOT NULL
   )`,
];

export type Surface = "chat" | "api" | "mcp";

export const dayOf = (ms: number) => new Date(ms).toISOString().slice(0, 10);

export function createLeaderboardStore(sql: Sql) {
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of LEADERBOARD_SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  return {
    init,

    /** Count one call. Fire and forget from request paths: a failure only loses a count. */
    async record(userId: string, surface: Surface, item: unknown, at = Date.now()): Promise<void> {
      await init();
      await sql.run(
        `INSERT INTO usage_daily (day, user_id, surface, item, calls) VALUES (?, ?, ?, ?, 1)
         ON CONFLICT(day, user_id, surface, item) DO UPDATE SET calls = calls + 1`,
        [dayOf(at), userId, surface, cleanLabel(item)],
      );
    },

    /**
     * Calls per CoinPay sub, surface and item since `sinceMs` (0 = all). The
     * sub is only ever turned into a token by the caller; it is never published.
     */
    async usageSince(sinceMs: number): Promise<{ sub: string; surface: Surface; item: string; calls: number }[]> {
      await init();
      const rows = await sql.all(
        `SELECT u.coinpay_sub AS sub, d.surface AS surface, d.item AS item, SUM(d.calls) AS calls
           FROM usage_daily d JOIN chat_users u ON u.id = d.user_id
          WHERE d.day >= ?
          GROUP BY u.coinpay_sub, d.surface, d.item`,
        [sinceMs > 0 ? dayOf(sinceMs) : "0000-00-00"],
      );
      return rows
        .filter((r) => typeof r.sub === "string" && r.sub)
        .map((r) => ({ sub: String(r.sub), surface: String(r.surface) as Surface, item: String(r.item), calls: Number(r.calls) || 0 }));
    },

    async firstDay(): Promise<string | null> {
      await init();
      const r = (await sql.all(`SELECT MIN(day) AS d FROM usage_daily`))[0];
      return typeof r?.d === "string" ? r.d : null;
    },

    // ── opt-out ──────────────────────────────────────────────────────────────
    async isOptedOut(userId: string): Promise<boolean> {
      await init();
      return (await sql.all(`SELECT 1 AS x FROM leaderboard_optout WHERE user_id = ?`, [userId])).length > 0;
    },

    async setOptOut(userId: string, hidden: boolean): Promise<void> {
      await init();
      if (hidden) await sql.run(`INSERT OR IGNORE INTO leaderboard_optout (user_id, created_at) VALUES (?, ?)`, [userId, new Date().toISOString()]);
      else await sql.run(`DELETE FROM leaderboard_optout WHERE user_id = ?`, [userId]);
    },

    /**
     * Who is hidden: their CoinPay subs, and the nodes they claimed from the
     * node itself (via "key") or through hosting. A claim typed into a browser
     * is self-asserted, so it can't hide a node: otherwise anyone could pull a
     * stranger's node off the board.
     */
    async hidden(): Promise<{ subs: string[]; nodes: string[] }> {
      await init();
      const subs = (await sql.all(`SELECT u.coinpay_sub AS sub FROM leaderboard_optout o JOIN chat_users u ON u.id = o.user_id`))
        .map((r) => String(r.sub ?? ""))
        .filter(Boolean);
      let nodes: string[] = [];
      try {
        nodes = (
          await sql.all(
            `SELECT n.node AS node FROM operator_nodes n JOIN leaderboard_optout o ON o.user_id = n.user_id WHERE n.via IN ('key', 'hosting')`,
          )
        ).map((r) => String(r.node));
      } catch {
        /* no operator_nodes table yet: nobody has claimed a node */
      }
      return { subs, nodes };
    },
  };
}

export type LeaderboardStore = ReturnType<typeof createLeaderboardStore>;

/**
 * Per-node job history since `sinceMs` from the node_jobs ledger (the table
 * the earnings recorder fills; the caller makes sure it exists). Jobs whose
 * pay is settled in Infernet's ledger (`settledElsewhere`) still count as
 * jobs served by that node, but not as native jobs or native pay: the
 * Infernet ledger counts those, once.
 */
export async function nodeWindowStats(
  sql: Sql,
  sinceMs: number,
  settledElsewhere: readonly string[],
): Promise<{ node: string; jobs: number; busy_s: number; native_jobs: number; native_paid_usd: number; active_days: number }[]> {
  const inList = settledElsewhere.length ? settledElsewhere.map(() => "?").join(", ") : "''";
  const rows = await sql.all(
    `SELECT node,
            COUNT(*) AS jobs,
            SUM(busy_s) AS busy_s,
            SUM(CASE WHEN type IN (${inList}) THEN 0 ELSE 1 END) AS native_jobs,
            SUM(CASE WHEN type IN (${inList}) THEN 0 ELSE paid_usd END) AS native_paid_usd,
            COUNT(DISTINCT completed_at / 86400000) AS active_days
       FROM node_jobs WHERE completed_at >= ? GROUP BY node`,
    [...settledElsewhere, ...settledElsewhere, sinceMs],
  );
  return rows.map((r) => ({
    node: String(r.node),
    jobs: Number(r.jobs) || 0,
    busy_s: Number(r.busy_s) || 0,
    native_jobs: Number(r.native_jobs) || 0,
    native_paid_usd: Number(r.native_paid_usd) || 0,
    active_days: Number(r.active_days) || 0,
  }));
}
