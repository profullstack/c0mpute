import { Database } from "bun:sqlite";
import { describe, expect, test } from "bun:test";
import { Router, buildProfiles, MemoryStore, type Snapshot } from "@/lib/router";
import { completionJson, routeRequest, routedChat, routedStream, sqlStatsStore, teeForLearning } from "./routed";
import type { Sql } from "./store";

const pool = buildProfiles(["qwen2.5:0.5b", "qwen2.5:7b", "qwen3:4b", "qwen2.5-coder:7b"]);

function sqliteSql(): Sql {
  const db = new Database(":memory:");
  return {
    async all(q, p = []) {
      return db.query(q).all(...(p as never[])) as Record<string, unknown>[];
    },
    async run(q, p = []) {
      db.query(q).run(...(p as never[]));
    },
  };
}

const sseOf = (parts: string[]) =>
  new ReadableStream<Uint8Array>({
    start(c) {
      const e = new TextEncoder();
      for (const p of parts) c.enqueue(e.encode(p));
      c.close();
    },
  });

describe("routeRequest", () => {
  test("auto, aliases and absent model route; named models do not", () => {
    expect(routeRequest({ model: "auto" })?.effort).toBe("standard");
    expect(routeRequest({})?.effort).toBe("standard");
    expect(routeRequest({ model: "auto:max" })?.effort).toBe("max");
    expect(routeRequest({ model: "qwen2.5:7b" })).toBeNull();
    const r = routeRequest({ model: "auto", router: { effort: "high", max_cost_usd: 0.01, max_latency_ms: 999_999 } });
    expect(r).toEqual({ effort: "high", maxCostUsd: 0.01, maxLatencyMs: 290_000 });
  });
});

describe("persistence", () => {
  test("stats survive a round trip through SQL", async () => {
    const store = sqlStatsStore(sqliteSql());
    expect(await store.load()).toBeNull();
    const snap: Snapshot = { v: 1, quality: { "m\u0001code": { ok: 3, fail: 1 } }, health: {} };
    await store.save(snap);
    await store.save({ ...snap, quality: { "m\u0001code": { ok: 4, fail: 1 } } });
    expect((await store.load())?.quality["m\u0001code"].ok).toBe(4);
  });
});

describe("responses", () => {
  test("a streamed reply passes through untouched and still teaches the router", async () => {
    const parts = [
      'data: {"choices":[{"delta":{"content":"Hel"}}]}\n\n',
      'data: {"choices":[{"delta":{"content":"lo there"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n',
    ];
    let seen = "";
    const out = await new Response(teeForLearning(sseOf(parts), (t) => (seen = t))).text();
    expect(out).toBe(parts.join(""));
    expect(seen).toBe("Hello there");
  });

  test("a routed SSE stream ends with the receipt and [DONE]", async () => {
    const r = new Router({ call: async () => ({ text: "Paris is the capital of France." }), models: async () => pool });
    const work = r.run([{ role: "user", content: "Capital of France?" }], { effort: "standard" });
    const body = await new Response(routedStream(work)).text();
    expect(body).toContain("Paris");
    expect(body).toContain("x_c0mpute_route");
    expect(body.trim().endsWith("data: [DONE]")).toBe(true);
  });

  test("JSON completions carry usage and the receipt", async () => {
    const r = new Router({ call: async () => ({ text: "hi", inTokens: 5, outTokens: 2 }), models: async () => pool });
    const { text, model, receipt } = await r.run([{ role: "user", content: "hi" }], { effort: "fast" });
    const j = completionJson(text, model, receipt);
    expect(j.choices[0].message.content).toBe("hi");
    expect(j.usage.total_tokens).toBe(7);
    expect(j.x_c0mpute_route.final.model).toBe(model);
  });

  test("routedChat: standard + stream goes straight through on the planned model", async () => {
    const r = new Router({ call: async () => ({ text: "x" }), models: async () => pool, store: new MemoryStore() });
    const seen: string[] = [];
    const upstream = async (_path: string, init?: RequestInit) => {
      seen.push(JSON.parse(String(init?.body)).model);
      return new Response(sseOf(['data: {"choices":[{"delta":{"content":"ok"}}]}\n\ndata: [DONE]\n\n']), { headers: { "content-type": "text/event-stream" } });
    };
    const res = await routedChat({ router: r, upstream, req: { messages: [{ role: "user", content: "hello" }], stream: true }, route: { effort: "standard" } });
    expect(res.headers.get("x-c0mpute-route")).toContain("pattern=single");
    expect(await res.text()).toContain("ok");
    expect(seen.length).toBe(1);
    expect(seen[0]).toBeTruthy();
  });
});
