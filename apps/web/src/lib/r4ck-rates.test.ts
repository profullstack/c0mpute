import { describe, expect, test } from "bun:test";
import { GPU_RATE_REFERENCES } from "./gpu-estimates";
import { cpuSearchParams, gpuRatesFrom, isStale, matchesGpu, median, normalizeGpuModel, type R4ckOffer } from "./r4ck-rates";
import { createRatesCache, fetchLiveRates } from "./r4ck-rates-server";
import type { Sql } from "./chat/store";

let id = 0;
const offer = (model: string, hourly: number, extra: Partial<{ count: number; vram_mb: number; stock: string; interval: string; provider: string }> = {}): R4ckOffer => ({
  id: ++id,
  provider: { slug: extra.provider ?? "vast-ai" },
  compute: { gpu: { model, count: extra.count ?? 1, vram_mb: extra.vram_mb ?? null } },
  price: { interval: extra.interval ?? "hour", hourly_usd: hourly },
  stock: extra.stock ?? "in_stock",
});

describe("normalizeGpuModel", () => {
  test("folds provider spellings together", () => {
    expect(normalizeGpuModel("NVIDIA GeForce RTX 4080 SUPER")).toBe("rtx 4080s");
    expect(normalizeGpuModel("NVIDIA RTX 4080S")).toBe("rtx 4080s");
    expect(normalizeGpuModel("NVIDIA RTX 6000Ada")).toBe("rtx 6000 ada");
    expect(normalizeGpuModel("NVIDIA RTX 6000 Ada Generation")).toBe("rtx 6000 ada");
    expect(normalizeGpuModel("NVIDIA A100-SXM4-80GB")).toBe("a100 sxm4");
    expect(normalizeGpuModel("AMD Instinct MI300X OAM")).toBe("mi300x");
    expect(normalizeGpuModel("NVIDIA NVIDIA_L40S")).toBe("l40s");
    expect(normalizeGpuModel("NVIDIA Tesla T4")).toBe("t4");
  });
});

describe("gpuRatesFrom", () => {
  const ref = (model: string) => GPU_RATE_REFERENCES.find((g) => g.model === model)!;

  test("median per GPU, out of stock and MIG slices left out", () => {
    const offers = [
      offer("NVIDIA RTX 4090", 0.3),
      offer("NVIDIA GeForce RTX 4090", 0.5, { provider: "runpod" }),
      offer("NVIDIA RTX 4090", 1.6, { count: 4 }), // 0.40 a GPU
      offer("NVIDIA RTX 4090", 0.01, { stock: "out_of_stock" }),
      offer("NVIDIA RTX 4090", 9, { interval: "month" }),
      offer("NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 1g.24gb", 0.2),
    ];
    const r = gpuRatesFrom(offers, [ref("NVIDIA RTX 4090"), ref("NVIDIA RTX PRO 6000 Blackwell")]);
    expect(r["NVIDIA RTX 4090"]).toEqual({ hourlyUsd: 0.4, lowUsd: 0.3, offers: 3, providers: 2 });
    expect(r["NVIDIA RTX PRO 6000 Blackwell"]).toBeUndefined();
  });

  test("variants stay apart: Ti, Super, and memory sizes", () => {
    const offers = [
      offer("NVIDIA RTX 4070S", 0.1),
      offer("NVIDIA RTX 4070S Ti", 0.2),
      offer("NVIDIA RTX 4060 Ti", 0.1, { vram_mb: 16380 }),
      offer("NVIDIA RTX 4060 Ti", 0.05, { vram_mb: 8188 }),
      offer("NVIDIA A100 SXM4", 0.6, { vram_mb: 40960 }),
      offer("NVIDIA A100 SXM4", 1.2, { vram_mb: 81920 }),
    ];
    const r = gpuRatesFrom(offers, GPU_RATE_REFERENCES);
    expect(r["NVIDIA RTX 4070 Super"].hourlyUsd).toBe(0.1);
    expect(r["NVIDIA RTX 4070 Ti Super"].hourlyUsd).toBe(0.2);
    expect(r["NVIDIA RTX 4070"]).toBeUndefined();
    expect(r["NVIDIA RTX 4060 Ti 16GB"]).toMatchObject({ hourlyUsd: 0.1, offers: 1 });
    expect(r["NVIDIA A100 SXM 40GB"].hourlyUsd).toBe(0.6);
    expect(r["NVIDIA A100 SXM"].hourlyUsd).toBe(1.2);
  });

  test("cards with no rental market never get a rate", () => {
    expect(GPU_RATE_REFERENCES.filter((g) => g.group === "Apple silicon").every((g) => g.r4ck === null)).toBe(true);
    expect(matchesGpu(offer("NVIDIA RTX 4090", 1), { pattern: /^rtx 4090$/, minVramGb: 20 })).toBe(false);
  });
});

test("median and staleness", () => {
  expect(median([1, 2, 3])).toBe(2);
  expect(median([1, 2, 3, 4])).toBe(2.5);
  expect(isStale(null, 1000)).toBe(true);
  expect(isStale({ fetchedAt: new Date(0).toISOString(), gpu: {}, cpu: {}, gpuOffersRead: 0 }, 1000, 500)).toBe(false);
  expect(isStale({ fetchedAt: new Date(0).toISOString(), gpu: {}, cpu: {}, gpuOffersRead: 0 }, 1000, 5000)).toBe(true);
});

test("cpu search asks for exactly that spec (r4ck max bounds are exclusive)", () => {
  const q = cpuSearchParams(2, 4);
  expect(q.get("min_vcpu")).toBe("2");
  expect(q.get("max_vcpu")).toBe("2.5");
  expect(q.get("min_ram")).toBe("3.75");
  expect(q.get("max_ram")).toBe("4.25");
});

test("fetchLiveRates pages GPU offers and reads the middle CPU plan", async () => {
  const calls: string[] = [];
  const fake = (async (url: string) => {
    calls.push(url);
    const u = new URL(url);
    if (u.searchParams.get("kind") === "gpu") {
      const off = Number(u.searchParams.get("offset"));
      const rows = off === 0 ? Array.from({ length: 50 }, () => offer("NVIDIA RTX 3090", 0.2)) : [offer("NVIDIA RTX 3090", 0.4)];
      return Response.json({ total: 51, servers: rows, next: off === 0 ? "more" : null });
    }
    const off = Number(u.searchParams.get("offset") ?? 0);
    return Response.json({ total: 9, servers: [{ id: 1, price: { hourly_usd: off ? 0.03 : 0.004 } }] });
  }) as unknown as typeof fetch;
  const r = await fetchLiveRates(fake, 0);
  expect(r.gpuOffersRead).toBe(51);
  expect(r.gpu["NVIDIA RTX 3090"]).toMatchObject({ hourlyUsd: 0.2, offers: 51 });
  expect(r.cpu["2 vCPUs / 4 GiB"]).toEqual({ hourlyUsd: 0.03, lowUsd: 0.004, offers: 9, providers: null });
  expect(calls.some((c) => c.includes("offset=4"))).toBe(true);
});

test("rates cache: serves the stored snapshot and refreshes in the background", async () => {
  const rows = new Map<string, string>();
  const sql: Sql = {
    all: async () => (rows.has("r4ck") ? [{ body: rows.get("r4ck") }] : []),
    run: async (q, p = []) => {
      if (q.startsWith("INSERT")) rows.set(String(p[0]), String(p[1]));
    },
  };
  let n = 0;
  const cache = createRatesCache(sql, async () => ({ fetchedAt: new Date().toISOString(), gpu: {}, cpu: {}, gpuOffersRead: ++n }));
  expect(await cache.get()).toBeNull();
  await cache.refresh();
  expect((await cache.get())?.gpuOffersRead).toBe(1);
  expect(n).toBe(1);
  const again = createRatesCache(sql, async () => {
    throw new Error("should not refresh a fresh snapshot");
  });
  expect((await again.get())?.gpuOffersRead).toBe(1);
});
