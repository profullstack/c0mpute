// The line of text under the rate box on /estimates, as pure functions so the
// wording is tested. Two different reasons can leave a card without a live
// r4ck rate, and they must not read the same:
// - no saved r4ck snapshot at all (r4ck unreachable, cache empty): live rates
//   are unavailable right now, whatever the card;
// - a snapshot that lists no offer for this card: r4ck really has none.
import { GPU_RATE_CHECKED_AT, GPU_RATE_SOURCES, type GpuRateReference } from "./gpu-estimates";
import { CPU_RATE_CHECKED_AT, CPU_RATE_SOURCE, type CpuRateReference } from "./cpu-estimates";
import { THIN_SAMPLE, type LiveRate, type LiveRates } from "./r4ck-rates";

export function asOf(live: LiveRates): string {
  return live.fetchedAt.slice(0, 10);
}

export function hourly(v: number): string {
  return `$${v < 0.1 ? v.toFixed(4).replace(/0+$/, "") : v < 1 ? v.toFixed(3).replace(/0$/, "") : v.toFixed(2)}/h`;
}

export function liveHint(r: LiveRate, live: LiveRates, what: string): string {
  const from = r.providers != null ? ` from ${r.providers} provider${r.providers === 1 ? "" : "s"}` : "";
  const thin = r.offers < THIN_SAMPLE ? ` Only ${r.offers} offer${r.offers === 1 ? "" : "s"}, so treat it as rough.` : "";
  return `median of ${r.offers.toLocaleString()} ${what}${from} on r4ck, as of ${asOf(live)}; cheapest ${hourly(r.lowUsd)}.${thin}`;
}

export function gpuRateHint(gpu: GpuRateReference | undefined, live: LiveRates | null): string {
  if (!gpu) return "Custom model: enter a comparable rate.";
  const rate = live?.gpu[gpu.model];
  if (rate && live) return `${gpu.vram} · per-GPU ${liveHint(rate, live, "hourly offers")}`;
  if (!gpu.r4ck && !gpu.source) return `${gpu.vram} · no public hourly rental rate for this GPU, so enter your own.`;
  const why = !gpu.r4ck
    ? ""
    : live
      ? "r4ck lists no live hourly offer for this GPU right now"
      : "live r4ck rates are unavailable right now";
  const source = gpu.source ? GPU_RATE_SOURCES[gpu.source] : null;
  if (source) {
    const fallback = `this is the ${source.label} rate checked ${GPU_RATE_CHECKED_AT}.`;
    return why ? `${gpu.vram} · ${why}, so ${fallback}` : `${gpu.vram} · ${fallback.charAt(0).toUpperCase()}${fallback.slice(1)}`;
  }
  return `${gpu.vram} · ${why}, so enter your own.`;
}

export function cpuRateHint(cpu: CpuRateReference | undefined, live: LiveRates | null): string {
  if (!cpu) return "Custom CPU VM: enter a comparable hourly price.";
  const rate = live?.cpu[cpu.model];
  if (rate && live) return `${cpu.specs} · monthly price over 730 h, ${liveHint(rate, live, "VPS plans with this spec")}`;
  const why = live ? "r4ck lists no live VPS plan with this spec right now" : "live r4ck rates are unavailable right now";
  return `${cpu.specs} · starting price from ${CPU_RATE_SOURCE.label}, checked ${CPU_RATE_CHECKED_AT} (${why}).`;
}
