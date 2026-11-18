import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import type { Sql } from "./chat/store";
import { createAutoscaleStore, fetchSignal, MAX_EVENTS, mergeSignals, parseReport, parseSignalQuery } from "./autoscale";

function memSql(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}

const PEER = "12D3KooWLxQZFJhZer1mywvnti85VeMTCRhJveunvYExsdNT3kCN";
const M = "eriktonon/qwen3.8-uncensored:9b";
const q = (s: string) => parseSignalQuery(new URLSearchParams(s));

describe("parseSignalQuery", () => {
  test("node, models and window", () => {
    expect(q(`node=${PEER}&models=${M},${M},b`)).toEqual({ node: "sdNT3kCN", peer_id: PEER, models: [M, "b"], window_s: 1200 });
    expect(q("node=sdNT3kCN&window_s=600")).toMatchObject({ node: "sdNT3kCN", models: [], window_s: 600 });
    expect(typeof q("node=bad")).toBe("string");
    expect(typeof q("node=sdNT3kCN&window_s=10")).toBe("string");
    expect(typeof q(`node=sdNT3kCN&models=${Array.from({ length: 17 }, (_, i) => `m${i}`).join(",")}`)).toBe("string");
    expect(typeof q(`node=sdNT3kCN&models=${"x".repeat(201)}`)).toBe("string");
  });
});

describe("mergeSignals", () => {
  const query = { node: "sdNT3kCN", peer_id: null, models: [M], window_s: 1200 };
  test("any online, freshest sighting, most work and demand", () => {
    const a = {
      node: { id: "sdNT3kCN", peer_id: null, online: false, last_seen_s: 40, in_flight: 0, last_accept_ms: 100, last_receipt_ms: null, serves: [M] },
      models: { [M]: { offers_window: 3, offers_2m: 0, waiting: 0, unserved_window: 1, last_offer_ms: 50 } },
    };
    const b = {
      node: { id: "sdNT3kCN", peer_id: PEER, online: true, last_seen_s: 5, in_flight: 2, last_accept_ms: 90, last_receipt_ms: 80, serves: ["b", M, 7] },
      models: { [M]: { offers_window: 2, offers_2m: 1, waiting: 1, unserved_window: 0, last_offer_ms: 70 } },
    };
    const s = mergeSignals([a, null, b, "junk"], query, 1000)!;
    expect(s.node).toEqual({ id: "sdNT3kCN", peer_id: PEER, online: true, last_seen_s: 5, in_flight: 2, last_accept_ms: 100, last_receipt_ms: 80, serves: ["b", M] });
    expect(s.models[M]).toEqual({ offers_window: 3, offers_2m: 1, waiting: 1, unserved_window: 1, last_offer_ms: 70 });
    expect(s.now_ms).toBe(1000);
  });
  test("a model no aggregator mentions is zero demand; junk numbers are unknown; none = null", () => {
    const s = mergeSignals([{ node: { online: true, in_flight: -4, last_seen_s: "x", peer_id: "12D3KooWsomeoneelse11111111" } }], query)!;
    expect(s.models[M]).toEqual({ offers_window: 0, offers_2m: 0, waiting: 0, unserved_window: 0, last_offer_ms: null });
    expect(s.node.in_flight).toBe(0);
    expect(s.node.last_seen_s).toBeNull();
    expect(s.node.peer_id).toBeNull(); // not this node's peer id
    expect(mergeSignals([null, {}], query)).toBeNull();
  });
  test("fetchSignal asks every aggregator and survives one failing", async () => {
    const urls: string[] = [];
    const fake = (async (u: string) => {
      urls.push(u);
      if (u.startsWith("http://a")) throw new Error("down");
      return new Response(JSON.stringify({ node: { online: true, in_flight: 1 }, models: {} }));
    }) as unknown as typeof fetch;
    const s = await fetchSignal(query, ["http://a", "http://b"], fake);
    expect(s?.node.in_flight).toBe(1);
    expect(urls[1]).toContain("/autoscale?node=sdNT3kCN");
  });
});

describe("parseReport", () => {
  const base = { node: PEER, provider: "Vast", instance: 61205210, state: "stopped", reason: "idle 20 min", at_ms: 1_000_000, saved_usd: 0.5 };
  test("valid report, normalized", () => {
    const r = parseReport({ ...base, running_usd_per_hour: 0.087, stopped_usd_per_hour: 0.033, idle_minutes: 20, models: [M, M] }, 1_000_000);
    expect(r).toMatchObject({ node: "sdNT3kCN", peer_id: PEER, provider: "vast", instance: "61205210", state: "stopped", saved_usd: 0.5, models: [M] });
  });
  test("future or ancient timestamps become now", () => {
    expect((parseReport({ ...base, at_ms: 9e15 }, 5_000) as { at_ms: number }).at_ms).toBe(5_000);
  });
  test("rejects junk", () => {
    expect(typeof parseReport(null)).toBe("string");
    expect(typeof parseReport({ ...base, node: "x" })).toBe("string");
    expect(typeof parseReport({ ...base, state: "exploded" })).toBe("string");
    expect(typeof parseReport({ ...base, provider: "a b" })).toBe("string");
    expect(typeof parseReport({ ...base, instance: "" })).toBe("string");
    expect(typeof parseReport({ ...base, saved_usd: -1 })).toBe("string");
    expect(typeof parseReport({ ...base, running_usd_per_hour: 5000 })).toBe("string");
    expect(typeof parseReport({ ...base, models: [1] })).toBe("string");
    expect((parseReport({ ...base, reason: "r".repeat(500) }) as { reason: string }).reason.length).toBe(300);
  });
});

describe("autoscale store", () => {
  test("latest state per node; events only on change; capped", async () => {
    const s = createAutoscaleStore(memSql());
    const rep = (state: string, reason: string, at: number) => parseReport({ node: PEER, provider: "vast", instance: "1", state, reason, at_ms: at, saved_usd: at / 1e6 }, at) as never;
    expect((await s.report("u1", rep("running", "start", 1_000), 1_000)).changed).toBe(true);
    expect((await s.report("u1", rep("running", "start", 2_000), 2_000)).changed).toBe(false);
    await s.report("u1", rep("stopped", "idle", 3_000), 3_000);
    const [n] = await s.nodes("u1");
    expect(n).toMatchObject({ node: "sdNT3kCN", state: "stopped", last_transition_ms: 3_000, reported_ms: 3_000, saved_usd: 0.003 });
    expect(n.events.map((e) => e.state)).toEqual(["stopped", "running"]);
    expect(await s.nodes("u2")).toEqual([]);
    for (let i = 0; i < MAX_EVENTS + 10; i++) await s.report("u1", rep(i % 2 ? "running" : "stopped", `r${i}`, 10_000 + i), 10_000 + i);
    const [m] = await s.nodes("u1");
    expect(m.events.length).toBe(20);
  });
});
