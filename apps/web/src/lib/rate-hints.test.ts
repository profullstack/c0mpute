import { describe, expect, test } from "bun:test";
import { GPU_RATE_REFERENCES } from "./gpu-estimates";
import { CPU_RATE_REFERENCES } from "./cpu-estimates";
import type { LiveRates } from "./r4ck-rates";
import { cpuRateHint, gpuRateHint } from "./rate-hints";

const gpu = (model: string) => {
  const g = GPU_RATE_REFERENCES.find((r) => r.model === model);
  if (!g) throw new Error(`no reference ${model}`);
  return g;
};
const withSource = GPU_RATE_REFERENCES.find((r) => r.r4ck && r.source)!;
const r4ckOnly = GPU_RATE_REFERENCES.find((r) => r.r4ck && !r.source)!;
const noMarket = GPU_RATE_REFERENCES.find((r) => !r.r4ck && !r.source)!;
const cpu = CPU_RATE_REFERENCES[0];

const emptySnapshot: LiveRates = { fetchedAt: "2026-11-22T06:00:00Z", gpu: {}, cpu: {}, gpuOffersRead: 1200 };
const liveSnapshot: LiveRates = {
  ...emptySnapshot,
  gpu: { [withSource.model]: { hourlyUsd: 0.35, lowUsd: 0.2, offers: 12, providers: 3 } },
  cpu: { [cpu.model]: { hourlyUsd: 0.007, lowUsd: 0.005, offers: 9, providers: null } },
};

describe("gpuRateHint", () => {
  test("no saved snapshot says live rates are unavailable, not that r4ck has no offer", () => {
    for (const g of [withSource, r4ckOnly]) {
      const hint = gpuRateHint(g, null);
      expect(hint).toContain("live r4ck rates are unavailable right now");
      expect(hint).not.toContain("lists no live");
    }
  });

  test("a snapshot with no offer for the card keeps the no-offer wording", () => {
    expect(gpuRateHint(withSource, emptySnapshot)).toContain("r4ck lists no live hourly offer for this GPU right now, so this is the");
    expect(gpuRateHint(r4ckOnly, emptySnapshot)).toBe(`${r4ckOnly.vram} · r4ck lists no live hourly offer for this GPU right now, so enter your own.`);
  });

  test("a live rate shows the median", () => {
    expect(gpuRateHint(withSource, liveSnapshot)).toContain("per-GPU median of 12 hourly offers from 3 providers on r4ck, as of 2026-11-22");
  });

  test("cards with no rental market never mention r4ck", () => {
    for (const live of [null, emptySnapshot]) {
      expect(gpuRateHint(noMarket, live)).toBe(`${noMarket.vram} · no public hourly rental rate for this GPU, so enter your own.`);
    }
    expect(gpuRateHint(undefined, null)).toBe("Custom model: enter a comparable rate.");
    expect(gpu(noMarket.model)).toBe(noMarket);
  });
});

describe("cpuRateHint", () => {
  test("distinguishes no snapshot from no matching plan", () => {
    expect(cpuRateHint(cpu, null)).toContain("(live r4ck rates are unavailable right now)");
    expect(cpuRateHint(cpu, emptySnapshot)).toContain("(r4ck lists no live VPS plan with this spec right now)");
    expect(cpuRateHint(cpu, liveSnapshot)).toContain("monthly price over 730 h, median of 9 VPS plans with this spec on r4ck");
  });
});
