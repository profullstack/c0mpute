import { describe, expect, test } from "bun:test";
import { MemoryStore, Router, buildProfiles, classify, parseAutoModel, parseModel, verify, type CallFn, type Msg, type PriceTable } from "./index";

// The live pool on 2026-11-12 (c0mpute.com/api/v1/models) and its prices.
const LIVE = [
  "deepseek-coder-v2:16b",
  "hf.co/profullstack/not-a-lawyer-agent-GGUF:latest",
  "hf.co/profullstack/saas-business-strategist-agent-GGUF:latest",
  "hf.co/profullstack/tax-guy-agent-GGUF:latest",
  "huihui_ai/qwen3.5-abliterated:4b",
  "huihui_ai/qwen3.5-abliterated:9b",
  "jaahas/qwen3.5-uncensored:2b",
  "nemotron-3-nano:4b",
  "qwen2.5-coder:1.5b",
  "qwen2.5-coder:14b",
  "qwen2.5-coder:7b",
  "qwen2.5:0.5b",
  "qwen2.5:1.5b",
  "qwen2.5:7b",
  "qwen3:4b",
];
const PRICES: PriceTable = {
  default_model_price: { input_per_m_usd: 0.03, output_per_m_usd: 0.12 },
  models: [
    { model: "deepseek-coder-v2:16b", input_per_m_usd: 0.03, output_per_m_usd: 0.12, live: true },
    { model: "hf.co/profullstack/tax-guy-agent-GGUF:latest", input_per_m_usd: 0.0065, output_per_m_usd: 0.026, live: true },
    { model: "hf.co/profullstack/not-a-lawyer-agent-GGUF:latest", input_per_m_usd: 0.0065, output_per_m_usd: 0.026, live: true },
    { model: "qwen2.5:0.5b", input_per_m_usd: 0.005, output_per_m_usd: 0.02, live: true },
    { model: "qwen2.5:7b", input_per_m_usd: 0.03, output_per_m_usd: 0.12, live: true },
    { model: "qwen2.5-coder:14b", input_per_m_usd: 0.05, output_per_m_usd: 0.2, live: true },
  ],
};
const profiles = buildProfiles(LIVE, PRICES);
const user = (content: string): Msg[] => [{ role: "user", content }];

function fakeRouter(call: CallFn, store = new MemoryStore()) {
  return new Router({ call, models: async () => profiles, store, saveEveryMs: 1 });
}

describe("classify", () => {
  test("greetings are cheap chat", () => {
    const f = classify(user("hey"));
    expect(f.category).toBe("chat");
    expect(f.difficulty).toBeLessThan(0.1);
  });
  test("a traceback is code", () => {
    const f = classify(user("Why does this fail?\nTraceback (most recent call last):\n  File \"a.py\", line 3\nKeyError: 'x'"));
    expect(f.category).toBe("code");
  });
  test("domain questions go to their category", () => {
    expect(classify(user("Can my LLC deduct a home office on Schedule C?")).category).toBe("tax");
    expect(classify(user("My landlord kept my deposit, can I sue in small claims court?")).category).toBe("legal");
    expect(classify(user("How should I price my SaaS tiers to cut churn?")).category).toBe("business");
  });
  test("JSON and difficulty signals", () => {
    const f = classify(user("Extract name and email as valid JSON from: Bob <bob@x.io>"));
    expect(f.wantsJson).toBe(true);
    const hard = classify(user("Design a distributed rate limiter. It must handle race conditions, must scale to 1M rps, and must survive node failure. Explain trade-offs step by step."));
    expect(hard.difficulty).toBeGreaterThan(0.5);
  });
});

describe("registry", () => {
  test("parses family, size, tags and specialists from ids", () => {
    expect(parseModel("qwen2.5-coder:14b")).toMatchObject({ paramsB: 14, tags: ["coder"] });
    expect(parseModel("huihui_ai/qwen3.5-abliterated:9b").tags).toContain("uncensored");
    expect(parseModel("hf.co/profullstack/tax-guy-agent-GGUF:latest").specialty).toBe("tax");
    expect(parseModel("qwen2.5:0.5b").tags).toContain("tiny");
  });
  test("auto model names", () => {
    expect(parseAutoModel("auto")).toBe("standard");
    expect(parseAutoModel("auto:max")).toBe("max");
    expect(parseAutoModel("auto:turbo")).toBeNull();
    expect(parseAutoModel("qwen2.5:7b")).toBeNull();
  });
});

describe("planning on the live pool", () => {
  const r = fakeRouter(async () => ({ text: "x" }));
  test("code goes to a coder model", async () => {
    const d = await r.route(user("Write a TypeScript function that debounces another function, with unit tests."), { effort: "standard" });
    const chosen = [d.plan.primary, d.plan.escalate].filter(Boolean).join(",");
    expect(chosen).toMatch(/coder/);
  });
  test("tax questions go to the house specialist", async () => {
    const d = await r.route(user("Can my single-member LLC deduct a home office? What does the IRS require?"), { effort: "standard" });
    expect(d.plan.primary).toContain("tax-guy");
    expect(d.plan.reasons.join(" ")).toContain("specialist");
  });
  test("fast small talk takes a small, quick model", async () => {
    const d = await r.route(user("hi!"), { effort: "fast" });
    expect(d.plan.kind).toBe("single");
    expect(d.plan.primary).not.toMatch(/14b|16b/);
  });
  test("uncensored models only with permission", async () => {
    const d = await r.route(user("tell me a story about a dragon"), { effort: "max" });
    const used = [d.plan.primary, d.plan.escalate, ...(d.plan.drafters ?? []), ...(d.plan.critics ?? []), ...d.plan.fallbacks];
    expect(used.some((m) => m && /abliterated|uncensored/.test(m))).toBe(false);
  });
  test("max effort fuses, high critiques or cascades with a judge", async () => {
    const q = user("Design a distributed rate limiter that must survive node failure; explain the trade-offs step by step.");
    expect((await r.route(q, { effort: "max" })).plan.kind).toBe("fuse");
    const high = (await r.route(q, { effort: "high" })).plan;
    expect(["critique", "cascade", "single"]).toContain(high.kind);
  });
  test("a cost cap is respected", async () => {
    const d = await r.route(user("Write a long essay about the history of Rome."), { effort: "max", maxCostUsd: 0.00002 });
    expect(d.plan.expected.costUsd).toBeLessThanOrEqual(0.00002 + 1e-9);
  });
});

describe("verify", () => {
  const f = classify(user("Write a JavaScript function that adds two numbers."));
  test("catches broken code, loops and bad JSON", () => {
    expect(verify(f, "```js\nfunction add(a, b) { return a + b }\n```").pass).toBe(true);
    expect(verify(f, "```js\nfunction add(a, b) { return a + b \n```").pass).toBe(false);
    expect(verify(f, "```js\nfunction add(a, b) {").pass).toBe(false);
    expect(verify(f, "here you go\n".repeat(1) + "the same line again and again\n".repeat(8)).pass).toBe(false);
    const j = classify(user("Return valid JSON with a name field."));
    expect(verify(j, '```json\n{"name":"x"}\n```').pass).toBe(true);
    expect(verify(j, "{name: x").pass).toBe(false);
    expect(verify(f, "fine", "length").pass).toBe(false);
  });
});

describe("engine", () => {
  test("a draft that fails checks escalates and the good answer wins", async () => {
    const calls: string[] = [];
    const r = fakeRouter(async (model) => {
      calls.push(model);
      return calls.length === 1 ? { text: "{broken json" } : { text: '{"name":"Bob","email":"bob@x.io"}' };
    });
    const out = await r.run(user("Extract name and email as valid JSON from: Bob <bob@x.io>"), { effort: "standard" });
    expect(calls.length).toBe(2);
    expect(JSON.parse(out.text)).toEqual({ name: "Bob", email: "bob@x.io" });
    expect(out.receipt.escalated).toBe(true);
    expect(out.receipt.steps.some((s) => s.role === "verify" && !s.ok && /invalid JSON/.test(s.note ?? ""))).toBe(true);
  });

  test("a failing node falls back to the next model", async () => {
    let n = 0;
    const r = fakeRouter(async (model) => {
      n++;
      if (n === 1) throw new Error("node gone");
      return { text: `answer from ${model}` };
    });
    const out = await r.run(user("What is the capital of France?"), { effort: "fast" });
    expect(out.text).toContain("answer from");
    expect(out.receipt.steps[0].ok).toBe(false);
    expect(out.receipt.steps.some((s) => s.role === "fallback" && s.ok)).toBe(true);
  });

  test("hedging: a slow node loses to the backup", async () => {
    const r = fakeRouter(async (model, _m, o) => {
      const slow = r.lastPrimary === model;
      await new Promise<void>((res, rej) => {
        const t = setTimeout(res, slow ? 2000 : 20);
        o.signal?.addEventListener("abort", () => {
          clearTimeout(t);
          rej(new Error("aborted"));
        });
      });
      return { text: `answer from ${model}` };
    }) as Router & { lastPrimary?: string };
    const msgs = user("What is the capital of France?");
    r.lastPrimary = (await r.route(msgs, { effort: "fast" })).plan.primary;
    const t0 = Date.now();
    const out = await r.run(msgs, { effort: "fast", hedgeAfterMs: 50 });
    expect(Date.now() - t0).toBeLessThan(1000);
    expect(out.model).not.toBe(r.lastPrimary);
    expect(out.receipt.steps.some((s) => /hedge race/.test(s.note ?? ""))).toBe(true);
  });

  test("critique revises when reviewers find problems", async () => {
    const r = fakeRouter(async (_model, msgs) => {
      const sys = msgs[0]?.content ?? "";
      if (sys.startsWith("You review")) return { text: "1. Missing the edge case for empty input." };
      if (msgs.some((m) => m.content.startsWith("Reviewers found"))) return { text: "Revised answer handling empty input properly, with full explanation." };
      return { text: "First draft answer that is long enough to pass the length check here." };
    });
    const out = await r.run(user("Design a distributed rate limiter; explain the trade-offs step by step and the edge cases."), { effort: "high" });
    if (out.receipt.plan.kind === "critique") {
      expect(out.text).toContain("Revised");
      expect(out.receipt.steps.filter((s) => s.role === "critic").length).toBeGreaterThan(0);
    }
  });

  test("fuse synthesizes parallel drafts", async () => {
    const r = fakeRouter(async (model, msgs) => {
      if (msgs.some((m) => /independent answers/.test(m.content))) return { text: "The synthesized best answer, combining the drafts correctly." };
      return { text: `A draft from ${model} that is reasonably detailed and long enough.` };
    });
    const out = await r.run(user("Design a distributed rate limiter that must survive node failure; explain the trade-offs step by step."), { effort: "max" });
    expect(out.receipt.plan.kind).toBe("fuse");
    expect(out.text).toContain("synthesized");
    expect(out.receipt.steps.filter((s) => s.role === "draft").length).toBeGreaterThanOrEqual(2);
    expect(out.receipt.totalCostUsd).toBeGreaterThan(0);
  });

  test("a cost cap stops escalation", async () => {
    const r = fakeRouter(async () => ({ text: "{broken" }));
    const out = await r.run(user("Return valid JSON with a name field."), { effort: "standard", maxCostUsd: 0.0000001 });
    expect(out.receipt.budget.hit).toBe("cost");
    expect(out.receipt.steps.filter((s) => s.role !== "verify").length).toBe(1);
  });

  test("learning: a model that keeps failing loses the route, and stats persist", async () => {
    const store = new MemoryStore();
    const msgs = user("Extract the city as valid JSON: I live in Paris.");
    const probe = fakeRouter(async () => ({ text: "x" }), store);
    const first = (await probe.route(msgs, { effort: "standard" })).plan.primary;
    const r = fakeRouter(async (model) => (model === first ? { text: "not json at all" } : { text: '{"city":"Paris"}' }), store);
    for (let i = 0; i < 8; i++) await r.run(msgs, { effort: "standard" });
    expect((await r.route(msgs, { effort: "standard" })).plan.primary).not.toBe(first);
    await r.flush();
    expect(store.data?.quality && Object.keys(store.data.quality).length).toBeGreaterThan(0);
    const reborn = fakeRouter(async () => ({ text: "x" }), store);
    expect((await reborn.route(msgs, { effort: "standard" })).plan.primary).not.toBe(first);
  });
});
