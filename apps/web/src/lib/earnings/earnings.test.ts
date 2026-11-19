import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import type { Sql } from "../chat/store";
import { cleanJob, createEarningsStore, nodeKey } from "./store";
import { buildReport, parseCost, parseRange } from "./report";
import { recordFrom } from "./recorder";

function memSql(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}

const PEER = "12D3KooWSFxwveBjrGy4n1Djg3vpQD5aHPCXYCQHZWruffig7P4J";
const H = 3_600_000;

describe("nodeKey", () => {
  test("full peer id or board id -> the board's 8-char row id", () => {
    expect(nodeKey(PEER)).toEqual({ node: "ffig7P4J", peer_id: PEER });
    expect(nodeKey(" ffig7P4J ")).toEqual({ node: "ffig7P4J", peer_id: null });
    expect(nodeKey("short")).toBeNull();
    expect(nodeKey("0OIl0OIl")).toBeNull(); // not base58
    expect(nodeKey(42)).toBeNull();
  });
});

describe("cleanJob", () => {
  test("keeps sane rows, caps numbers, drops junk", () => {
    const ok = cleanJob({ job_id: "j1", node: "ffig7P4J", type: "tts.synthesize", completed_at_ms: 1000, seen_at_ms: 1001, busy_s: 4, paid_usd: 5000 });
    expect(ok?.paid_usd).toBe(1000);
    expect(cleanJob({ job_id: "j1", node: "<script>", completed_at_ms: 1, seen_at_ms: 1 })).toBeNull();
    expect(cleanJob({ node: "ffig7P4J", completed_at_ms: 1, seen_at_ms: 1 })).toBeNull();
    expect(cleanJob({ job_id: "j", node: "ffig7P4J", type: "x y", completed_at_ms: 1, seen_at_ms: 1, busy_s: -3 })).toMatchObject({ type: "unknown", busy_s: 0 });
  });
});

describe("store", () => {
  test("jobs file once by id and bucket per node and type", async () => {
    const s = createEarningsStore(memSql());
    const j = (id: string, node: string, at: number, paid = 0.01) => ({ job_id: id, node, type: "tts.synthesize", completed_at_ms: at, seen_at_ms: at, busy_s: 10, paid_usd: paid });
    await s.recordJobs([j("a", "ffig7P4J", 10 * H + 1), j("b", "ffig7P4J", 10 * H + 2), j("c", "XtPja8R9", 11 * H)]);
    await s.recordJobs([j("a", "ffig7P4J", 10 * H + 1)]); // a second aggregator saw it too
    const rows = await s.buckets(["ffig7P4J"], 0, H);
    expect(rows).toEqual([{ node: "ffig7P4J", type: "tts.synthesize", bucket: 10, jobs: 2, busy_s: 20, paid_usd: 0.02 }]);
    expect((await s.buckets(null, 0, H)).length).toBe(2);
    expect(await s.buckets([], 0, H)).toEqual([]);
    expect(await s.firstJobAt()).toBe(10 * H + 1);
  });

  test("claims: upsert, key outranks web, cost, unclaim", async () => {
    const s = createEarningsStore(memSql());
    await s.claim("u1", { node: "ffig7P4J", via: "web" });
    const c = await s.claim("u1", { node: "ffig7P4J", peer_id: PEER, via: "key" });
    expect(c).toMatchObject({ via: "key", peer_id: PEER, cost_usd_per_hour: null });
    expect((await s.claim("u1", { node: "ffig7P4J", via: "web" })).via).toBe("key");
    expect(await s.update("u1", "ffig7P4J", { cost_usd_per_hour: 0.087 })).toBe(true);
    expect((await s.claims("u1"))[0].cost_usd_per_hour).toBe(0.087);
    expect(await s.claims("u2")).toEqual([]);
    expect(await s.update("u2", "ffig7P4J", { label: "x" })).toBe(false);
    expect(await s.unclaim("u1", "ffig7P4J")).toBe(true);
    expect(await s.unclaim("u1", "ffig7P4J")).toBe(false);
  });
});

describe("recordFrom", () => {
  test("pages with the cursor and skips aggregators without /jobs", async () => {
    const s = createEarningsStore(memSql());
    const pages = [
      { jobs: [{ job_id: "a", node: "ffig7P4J", type: "t", completed_at_ms: 5, seen_at_ms: 5, busy_s: 1, paid_usd: 0.1 }], next_since: 5, more: true },
      { jobs: [{ job_id: "b", node: "ffig7P4J", type: "t", completed_at_ms: 6, seen_at_ms: 6, busy_s: 1, paid_usd: 0.1 }], next_since: 6, more: false },
    ];
    const asked: string[] = [];
    const fetcher = (async (url: string) => {
      asked.push(url);
      return new Response(JSON.stringify(pages.shift() ?? { jobs: [], next_since: 6, more: false }));
    }) as unknown as typeof fetch;
    expect(await recordFrom("http://agg", s, fetcher)).toBe(2);
    expect(asked).toEqual(["http://agg/jobs?since=0", "http://agg/jobs?since=5"]);
    expect(await s.cursor("http://agg")).toBe(6);
    await recordFrom("http://agg", s, fetcher);
    expect(asked.at(-1)).toBe("http://agg/jobs?since=6");
    const old = (async () => new Response("not found", { status: 404 })) as unknown as typeof fetch;
    expect(await recordFrom("http://old", s, old)).toBe(0);
  });
});

describe("report", () => {
  const now = 100 * H + 30 * 60_000; // half past an hour
  const node = (o: Partial<{ node: string; class: string; cost_usd_per_hour: number | null }> = {}) => ({
    node: "A",
    label: null,
    class: "cpu",
    cost_usd_per_hour: null,
    online: true,
    via: "web",
    ...o,
  });
  test("auction prices are only 'agreed', never earned; no cost set means $0 spend", () => {
    const r = buildReport({
      range: "3h",
      now,
      nodes: [node()],
      rows: [{ node: "A", type: "tts.synthesize", bucket: Math.floor((now - 10 * 60_000) / 900_000), jobs: 3, busy_s: 3600, paid_usd: 1 }],
    });
    expect(r.buckets.length).toBe(12);
    expect(r.totals.jobs).toBe(3);
    expect(r.totals.agreed_usd).toBeCloseTo(0.95, 6);
    expect(r.totals.earned_usd).toBe(0);
    expect(r.totals.spend_usd).toBe(0);
    expect(r.totals.net_usd).toBe(0);
    expect(r.nodes[0].spend_basis).toBe("none");
    expect(r.nodes[0].utilization).toBeCloseTo(1 / 3, 6);
    expect(r.by_type[0]).toMatchObject({ type: "tts.synthesize", jobs: 3 });
  });

  test("a fixed cost accrues over the whole window, busy or not", () => {
    const r = buildReport({ range: "3h", now, nodes: [node({ node: "V", class: "gpu:nvidia", cost_usd_per_hour: 0.087 })], rows: [] });
    expect(r.totals.spend_usd).toBeCloseTo(0.087 * 3, 6);
    expect(r.totals.net_usd).toBeCloseTo(-0.087 * 3, 6);
    expect(r.nodes[0].spend_basis).toBe("fixed");
  });

  test("settled pay lands on the c0mpute node; self-paid jobs are split out of earned and net", () => {
    const r = buildReport({
      range: "3h",
      now,
      nodes: [node({ node: "sdNT3kCN" })],
      rows: [],
      infernet: {
        linked: true,
        providers: [{ id: "pc", name: "c0mpute sdNT3kCN", c0mpute_node: "sdNT3kCN" }],
        hours: [{ provider_id: "pc", c0mpute_node: "sdNT3kCN", at: 99 * H, jobs: 100, earned_usd: 0.095, own_jobs: 90, own_usd: 0.0855 }],
      },
    });
    expect(r.totals.earned_usd).toBeCloseTo(0.0095, 8);
    expect(r.totals.own_usd).toBeCloseTo(0.0855, 8);
    expect(r.totals.paid_jobs).toBe(10);
    expect(r.totals.own_jobs).toBe(90);
    expect(r.nodes).toHaveLength(1);
    expect(r.nodes[0]).toMatchObject({ node: "sdNT3kCN", paid_jobs: 10, own_jobs: 90 });
    expect(r.totals.net_usd).toBeCloseTo(0.0095, 8);
  });

  test("Infernet-only providers get their own row when they earned; rows for unclaimed nodes are ignored", () => {
    const r = buildReport({
      range: "1h",
      now,
      nodes: [node({ online: false })],
      rows: [{ node: "Z", type: "t", bucket: Math.floor(now / 300_000), jobs: 9, busy_s: 9, paid_usd: 9 }],
      infernet: {
        linked: true,
        providers: [
          { id: "p", name: "provider@bonita", c0mpute_node: null },
          { id: "idle", name: "idle box", c0mpute_node: null },
        ],
        hours: [{ provider_id: "p", c0mpute_node: null, at: 100 * H, jobs: 10, earned_usd: 1, own_jobs: 0, own_usd: 0 }],
      },
    });
    // The window is 99:30-100:30, so half of that hour counts.
    expect(r.totals.earned_usd).toBeCloseTo(0.5, 6);
    expect(r.nodes.map((n) => [n.label, n.kind])).toEqual([
      ["provider@bonita", "infernet"],
      [null, "c0mpute"],
    ]);
    expect(r.nodes.find((n) => n.node === "A")?.jobs).toBe(0);
  });

  test("parseCost and parseRange", () => {
    expect(parseCost("0.087", "hour")).toBe(0.087);
    expect(parseCost("$73", "month")).toBeCloseTo(0.1, 6);
    expect(parseCost("", "hour")).toBeNull();
    expect(parseCost(-1, "hour")).toBe("invalid");
    expect(parseCost(5, "year")).toBe("invalid");
    expect(parseRange("7d")).toBe("7d");
    expect(parseRange("1y")).toBe("24h");
  });
});
