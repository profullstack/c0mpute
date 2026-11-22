import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import type { Sql } from "../chat/store";
import { createEarningsStore } from "./store";
import { parseInfernetEarnings, summariseNodeTotals } from "./network";

function memSql(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}

describe("summariseNodeTotals", () => {
  test("totals after the market fee, best and median, free jobs counted but unpaid", () => {
    const s = summariseNodeTotals([
      { node: "aaaaaaaa", jobs: 10, paid_usd: 1.0 },
      { node: "bbbbbbbb", jobs: 4, paid_usd: 0.2 },
      { node: "cccccccc", jobs: 3, paid_usd: 0.4 },
      { node: "dddddddd", jobs: 50, paid_usd: 0 }, // free jobs only
    ]);
    expect(s.jobs).toBe(67);
    expect(s.paid_jobs).toBe(17);
    expect(s.gross_usd).toBeCloseTo(1.6, 6);
    expect(s.earned_usd).toBeCloseTo(1.52, 6); // 95%
    expect(s.fee_usd).toBeCloseTo(0.08, 6);
    expect(s.earning_nodes).toBe(3);
    expect(s.best_node_usd).toBeCloseTo(0.95, 6);
    expect(s.median_node_usd).toBeCloseTo(0.38, 6);
    expect(JSON.stringify(s)).not.toContain("aaaaaaaa");
  });

  test("empty is zeros", () => {
    expect(summariseNodeTotals([])).toMatchObject({ jobs: 0, earned_usd: 0, earning_nodes: 0, median_node_usd: 0 });
  });
});

describe("parseInfernetEarnings", () => {
  test("reads all_time and windows, clamps junk", () => {
    const p = parseInfernetEarnings({
      all_time: { earned_usd: 19.77, earning_nodes: 15, house_fee_usd: 1.07, house_fee_pct: 5.1, billed_usd: 20.84, commissions_usd: 0, paid_out_usd: 0.77 },
      windows: { "24h": { earned_usd: -5 }, "7d": { earned_usd: 17.9, earning_nodes: 5 } },
    });
    expect(p?.all.earned_usd).toBe(19.77);
    expect(p?.all.paid_out_usd).toBe(0.77);
    expect(p?.["24h"].earned_usd).toBe(0);
    expect(p?.["7d"].earning_nodes).toBe(5);
    expect(p?.["30d"].earned_usd).toBe(0); // missing window: zero, not all-time
  });
  test("garbage is null", () => {
    expect(parseInfernetEarnings(null)).toBeNull();
    expect(parseInfernetEarnings({ error: "x" })).toBeNull();
  });
});

describe("store.nodeTotals", () => {
  test("groups by node since a time", async () => {
    const store = createEarningsStore(memSql());
    await store.recordJobs([
      { job_id: "1", node: "aaaaaaaa", type: "tts", completed_at_ms: 1_000, seen_at_ms: 1_000, busy_s: 1, paid_usd: 0.1 },
      { job_id: "2", node: "aaaaaaaa", type: "tts", completed_at_ms: 5_000, seen_at_ms: 5_000, busy_s: 1, paid_usd: 0.2 },
      { job_id: "3", node: "bbbbbbbb", type: "stt", completed_at_ms: 6_000, seen_at_ms: 6_000, busy_s: 1, paid_usd: 0 },
    ]);
    const all = await store.nodeTotals(0);
    expect(all.find((r) => r.node === "aaaaaaaa")).toEqual({ node: "aaaaaaaa", jobs: 2, paid_usd: 0.30000000000000004 });
    const recent = await store.nodeTotals(4_000);
    expect(recent.map((r) => [r.node, r.jobs]).sort()).toEqual([["aaaaaaaa", 1], ["bbbbbbbb", 1]]);
  });
});
