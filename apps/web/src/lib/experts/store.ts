// Experts trials and subscriptions in the site's SQLite Cloud database,
// against the two-method Sql interface (tests run it on bun:sqlite).
//
// Bound to the CoinPay `sub`, never an email: one trial per sub (the primary
// key is what makes it unfarmable: a second INSERT for the same sub is a
// no-op), and a subscription is a paid-until date per (sub, plan).

import { randomBytes } from "node:crypto";
import type { Sql, Row } from "../chat/store";

export const EXPERTS_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS expert_trials (
     coinpay_sub TEXT PRIMARY KEY,
     user_id     TEXT NOT NULL,
     started_at  TEXT NOT NULL,
     ends_at     TEXT NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS expert_subscriptions (
     coinpay_sub TEXT NOT NULL,
     plan        TEXT NOT NULL,
     user_id     TEXT NOT NULL,
     paid_until  TEXT NOT NULL,
     created_at  TEXT NOT NULL,
     updated_at  TEXT NOT NULL,
     PRIMARY KEY (coinpay_sub, plan)
   )`,
  `CREATE TABLE IF NOT EXISTS expert_checkouts (
     id                  TEXT PRIMARY KEY,
     coinpay_sub         TEXT NOT NULL,
     user_id             TEXT NOT NULL,
     plan                TEXT NOT NULL,
     amount_usd          REAL NOT NULL,
     period_days         INTEGER NOT NULL,
     checkout_session_id TEXT,
     paid_at             TEXT,
     created_at          TEXT NOT NULL
   )`,
  `CREATE INDEX IF NOT EXISTS idx_expert_checkouts_session ON expert_checkouts(checkout_session_id)`,
  `CREATE TABLE IF NOT EXISTS expert_usage (
     coinpay_sub TEXT NOT NULL,
     slug        TEXT NOT NULL,
     day         TEXT NOT NULL,
     n           INTEGER NOT NULL,
     PRIMARY KEY (coinpay_sub, slug, day)
   )`,
];

export type Trial = { coinpay_sub: string; user_id: string; started_at: string; ends_at: string };
export type Subscription = { coinpay_sub: string; plan: string; user_id: string; paid_until: string; created_at: string; updated_at: string };
export type Checkout = {
  id: string;
  coinpay_sub: string;
  user_id: string;
  plan: string;
  amount_usd: number;
  period_days: number;
  checkout_session_id: string | null;
  paid_at: string | null;
  created_at: string;
};

const s = (v: unknown) => (v === null || v === undefined ? null : String(v));
const toTrial = (r: Row): Trial => ({ coinpay_sub: String(r.coinpay_sub), user_id: String(r.user_id), started_at: String(r.started_at), ends_at: String(r.ends_at) });
const toSub = (r: Row): Subscription => ({
  coinpay_sub: String(r.coinpay_sub),
  plan: String(r.plan),
  user_id: String(r.user_id),
  paid_until: String(r.paid_until),
  created_at: String(r.created_at),
  updated_at: String(r.updated_at),
});
const toCheckout = (r: Row): Checkout => ({
  id: String(r.id),
  coinpay_sub: String(r.coinpay_sub),
  user_id: String(r.user_id),
  plan: String(r.plan),
  amount_usd: Number(r.amount_usd),
  period_days: Number(r.period_days),
  checkout_session_id: s(r.checkout_session_id),
  paid_at: s(r.paid_at),
  created_at: String(r.created_at),
});

export function createExpertsStore(sql: Sql) {
  let ready: Promise<void> | null = null;
  const ensure = () =>
    (ready ??= (async () => {
      for (const q of EXPERTS_SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  return {
    ensure,
    async trial(sub: string): Promise<Trial | null> {
      await ensure();
      const r = await sql.all(`SELECT * FROM expert_trials WHERE coinpay_sub = ?`, [sub]);
      return r[0] ? toTrial(r[0]) : null;
    },
    /**
     * Start the one trial this sub will ever get. Atomic: INSERT OR IGNORE on
     * the primary key, then read back whatever row is there (ours or an older one).
     */
    async startTrial(sub: string, userId: string, days: number, at: Date): Promise<{ trial: Trial; created: boolean }> {
      await ensure();
      const ends = new Date(at.getTime() + days * 86_400_000);
      const before = await sql.all(`SELECT 1 FROM expert_trials WHERE coinpay_sub = ?`, [sub]);
      await sql.run(`INSERT OR IGNORE INTO expert_trials (coinpay_sub, user_id, started_at, ends_at) VALUES (?, ?, ?, ?)`, [sub, userId, at.toISOString(), ends.toISOString()]);
      const r = await sql.all(`SELECT * FROM expert_trials WHERE coinpay_sub = ?`, [sub]);
      const trial = toTrial(r[0]);
      return { trial, created: before.length === 0 && trial.started_at === at.toISOString() };
    },
    async subscriptions(sub: string): Promise<Subscription[]> {
      await ensure();
      return (await sql.all(`SELECT * FROM expert_subscriptions WHERE coinpay_sub = ? ORDER BY plan`, [sub])).map(toSub);
    },
    /** Extend (sub, plan) by `days` from max(now, current paid_until): an early renewal stacks. */
    async extend(sub: string, plan: string, userId: string, days: number, at: Date): Promise<Subscription> {
      await ensure();
      const cur = (await sql.all(`SELECT * FROM expert_subscriptions WHERE coinpay_sub = ? AND plan = ?`, [sub, plan]))[0];
      const base = cur && Date.parse(String(cur.paid_until)) > at.getTime() ? new Date(String(cur.paid_until)) : at;
      const until = new Date(base.getTime() + days * 86_400_000).toISOString();
      const t = at.toISOString();
      await sql.run(
        `INSERT INTO expert_subscriptions (coinpay_sub, plan, user_id, paid_until, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (coinpay_sub, plan) DO UPDATE SET paid_until = excluded.paid_until, updated_at = excluded.updated_at, user_id = excluded.user_id`,
        [sub, plan, userId, until, t, t],
      );
      return toSub((await sql.all(`SELECT * FROM expert_subscriptions WHERE coinpay_sub = ? AND plan = ?`, [sub, plan]))[0]);
    },
    async createCheckout(c: Omit<Checkout, "id" | "checkout_session_id" | "paid_at" | "created_at">, at: Date): Promise<Checkout> {
      await ensure();
      const id = `xc_${randomBytes(9).toString("base64url")}`;
      await sql.run(
        `INSERT INTO expert_checkouts (id, coinpay_sub, user_id, plan, amount_usd, period_days, checkout_session_id, paid_at, created_at) VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, ?)`,
        [id, c.coinpay_sub, c.user_id, c.plan, c.amount_usd, c.period_days, at.toISOString()],
      );
      return (await this.checkout(id))!;
    },
    async checkout(id: string): Promise<Checkout | null> {
      await ensure();
      const r = await sql.all(`SELECT * FROM expert_checkouts WHERE id = ?`, [id]);
      return r[0] ? toCheckout(r[0]) : null;
    },
    async checkoutBySession(sessionId: string): Promise<Checkout | null> {
      await ensure();
      const r = await sql.all(`SELECT * FROM expert_checkouts WHERE checkout_session_id = ?`, [sessionId]);
      return r[0] ? toCheckout(r[0]) : null;
    },
    async setCheckoutSession(id: string, sessionId: string): Promise<void> {
      await ensure();
      await sql.run(`UPDATE expert_checkouts SET checkout_session_id = ? WHERE id = ?`, [sessionId, id]);
    },
    /** Mark paid once; false when it already was (a webhook retry). */
    async markPaid(id: string, at: Date): Promise<boolean> {
      await ensure();
      await sql.run(`UPDATE expert_checkouts SET paid_at = ? WHERE id = ? AND paid_at IS NULL`, [at.toISOString(), id]);
      const r = await sql.all(`SELECT paid_at FROM expert_checkouts WHERE id = ?`, [id]);
      return r[0]?.paid_at === at.toISOString();
    },
    async recentCheckouts(sub: string, limit = 10): Promise<Checkout[]> {
      await ensure();
      return (await sql.all(`SELECT * FROM expert_checkouts WHERE coinpay_sub = ? ORDER BY created_at DESC LIMIT ?`, [sub, limit])).map(toCheckout);
    },
    /** Count one question (per sub, expert, UTC day). */
    async countAsk(sub: string, slug: string, at: Date): Promise<void> {
      await ensure();
      await sql.run(
        `INSERT INTO expert_usage (coinpay_sub, slug, day, n) VALUES (?, ?, ?, 1) ON CONFLICT (coinpay_sub, slug, day) DO UPDATE SET n = n + 1`,
        [sub, slug, at.toISOString().slice(0, 10)],
      );
    },
    async usage(sub: string): Promise<Record<string, number>> {
      await ensure();
      const rows = await sql.all(`SELECT slug, SUM(n) AS n FROM expert_usage WHERE coinpay_sub = ? GROUP BY slug`, [sub]);
      return Object.fromEntries(rows.map((r) => [String(r.slug), Number(r.n)]));
    },
  };
}

export type ExpertsStore = ReturnType<typeof createExpertsStore>;
