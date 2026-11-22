import { expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import type { Sql } from "../chat/store";
import { buildObserved } from "./network-by-class";
import { createEarningsStore } from "./store";

test("buildObserved groups by class and workload, net of the fee, per paid hour", () => {
  const now = Date.UTC(2026, 10, 22);
  const w = buildObserved({
    days: 7,
    now,
    historyFrom: now - 4 * 86_400_000,
    classOf: (n) => ({ a: "gpu:nvidia", b: "cpu" })[n] ?? null,
    rows: [
      { node: "a", type: "infernet.inference", jobs: 10, busy_s: 3600, paid_usd: 1 },
      { node: "b", type: "tts.synthesize", jobs: 5, busy_s: 1800, paid_usd: 0.1 },
      { node: "c", type: "infernet.inference", jobs: 1, busy_s: 10, paid_usd: 0 },
    ],
  });
  expect(w.covered_days).toBe(4);
  expect(w.totals).toMatchObject({ nodes: 3, jobs: 16 });
  expect(w.totals.net_usd).toBeCloseTo(1.1 * 0.95);
  const gpu = w.by_class.find((g) => g.key === "gpu:nvidia")!;
  expect(gpu.net_per_busy_hour).toBeCloseTo(0.95);
  expect(w.by_class.find((g) => g.key === "unknown")?.net_per_busy_hour).toBeNull();
  expect(w.by_type[0]).toMatchObject({ key: "infernet.inference", label: "chat inference (Infernet)", nodes: 2, jobs: 11 });
});

test("store.networkSince and claimedClasses read the whole network", async () => {
  const db = new Database(":memory:");
  const sql: Sql = {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
  const store = createEarningsStore(sql);
  await store.recordJobs([
    { job_id: "1", node: "aaaaaaaa", type: "x", completed_at_ms: 100, seen_at_ms: 100, busy_s: 5, paid_usd: 0.5 },
    { job_id: "2", node: "aaaaaaaa", type: "x", completed_at_ms: 200, seen_at_ms: 200, busy_s: 5, paid_usd: 0.5 },
    { job_id: "3", node: "bbbbbbbb", type: "y", completed_at_ms: 50, seen_at_ms: 50, busy_s: 5, paid_usd: 0.5 },
  ]);
  expect(await store.networkSince(90)).toEqual([{ node: "aaaaaaaa", type: "x", jobs: 2, busy_s: 10, paid_usd: 1 }]);
  await store.claim("u1", { node: "aaaaaaaa", class: "gpu:nvidia", via: "web" });
  expect((await store.claimedClasses()).get("aaaaaaaa")).toBe("gpu:nvidia");
});
