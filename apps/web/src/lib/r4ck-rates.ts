// Live comparison rates from r4ck.dev, our search engine over server and GPU
// offers. Pure functions only (no I/O): the fetching and caching live in
// ./r4ck-rates-server.ts, so the arithmetic is tested on its own.
//
// GPU: every hourly (on-demand) GPU offer r4ck lists, matched to a reference
// card by its model name, priced per GPU (an 8x box at $16/h is $2/h a GPU).
// The rate is the median of those per-GPU prices. Out-of-stock offers are
// left out; MIG slices of a bigger card are not the card.
//
// CPU: the median monthly price of VPS plans with exactly that many vCPUs
// and that much RAM, over 730 hours.

export const R4CK_SITE = "https://r4ck.dev";
/** Below this many offers a median is shown, but flagged as thin. */
export const THIN_SAMPLE = 5;

/** One offer as r4ck's `/api/v1/search` returns it (only the fields we read). */
export interface R4ckOffer {
  id: number;
  provider?: { slug?: string } | null;
  compute?: { gpu?: { model?: string | null; count?: number | null; vram_mb?: number | null } | null } | null;
  price?: { interval?: string | null; hourly_usd?: number | null; monthly_usd?: number | null } | null;
  stock?: string | null;
}

/** How a reference card is recognised among r4ck's model names. */
export interface R4ckGpuMatch {
  /** Tested against the normalised model name (see normalizeGpuModel). */
  pattern: RegExp;
  /** Per-GPU memory bounds in GB, for cards sold in two sizes under one name. */
  minVramGb?: number;
  maxVramGb?: number;
}

export interface LiveRate {
  /** Median USD per hour (per GPU for GPUs). */
  hourlyUsd: number;
  /** Cheapest matching offer, USD per hour. */
  lowUsd: number;
  offers: number;
  /** Distinct providers behind the offers; null when not counted (CPU medians). */
  providers: number | null;
}

export interface LiveRates {
  /** ISO time the offers were read from r4ck. */
  fetchedAt: string;
  gpu: Record<string, LiveRate>;
  cpu: Record<string, LiveRate>;
  /** Hourly GPU offers read in total, matched or not. */
  gpuOffersRead: number;
}

/**
 * One spelling for the many ways providers write a GPU model:
 * "NVIDIA GeForce RTX 4080 SUPER" and "NVIDIA RTX 4080S" both become
 * "rtx 4080s"; "NVIDIA A100-SXM4-80GB" becomes "a100 sxm4" (memory comes from
 * vram_mb, not the name).
 */
export function normalizeGpuModel(raw: string): string {
  return ` ${raw.toLowerCase()} `
    .replace(/[_\-()/]+/g, " ")
    .replace(/\b(nvidia|geforce|tesla|amd|instinct|generation|edition|oam|hbm3e?|\d+\s?gb)\b/g, " ")
    .replace(/(\d)ada\b/g, "$1 ada")
    .replace(/\bsuper\b/g, "s")
    .replace(/(\d{4}) s\b/g, "$1s")
    .replace(/\s+/g, " ")
    .trim();
}

export function perGpuHourly(o: R4ckOffer): number | null {
  const p = o.price;
  if (!p || p.interval !== "hour") return null;
  const total = Number(p.hourly_usd);
  const count = Math.max(1, Number(o.compute?.gpu?.count) || 1);
  if (!Number.isFinite(total) || total <= 0) return null;
  return total / count;
}

export function median(sorted: number[]): number {
  const n = sorted.length;
  if (n === 0) return NaN;
  const mid = Math.floor(n / 2);
  return n % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

/** Round a price to something readable without hiding cents on cheap cards. */
export function roundRate(v: number): number {
  if (v >= 1) return Math.round(v * 100) / 100;
  if (v >= 0.1) return Math.round(v * 1000) / 1000;
  return Math.round(v * 100000) / 100000;
}

export function matchesGpu(o: R4ckOffer, m: R4ckGpuMatch): boolean {
  const model = o.compute?.gpu?.model;
  if (!model) return false;
  const name = normalizeGpuModel(model);
  if (/\bmig\b/.test(name) || !m.pattern.test(name)) return false;
  if (m.minVramGb != null || m.maxVramGb != null) {
    const vram = Number(o.compute?.gpu?.vram_mb);
    // Memory unknown: fine for an upper bound, not for a lower one.
    if (!Number.isFinite(vram) || vram <= 0) return m.minVramGb == null;
    const gb = vram / 1024;
    if (m.minVramGb != null && gb < m.minVramGb) return false;
    if (m.maxVramGb != null && gb > m.maxVramGb) return false;
  }
  return true;
}

/** Median per-GPU hourly rate for each reference card that has at least one live offer. */
export function gpuRatesFrom(
  offers: R4ckOffer[],
  refs: readonly { model: string; r4ck?: R4ckGpuMatch | null }[],
): Record<string, LiveRate> {
  const usable = new Map<number, R4ckOffer>();
  for (const o of offers) {
    if (o.stock === "out_of_stock" || o.stock === "discontinued") continue;
    if (perGpuHourly(o) === null) continue;
    usable.set(o.id, o);
  }
  const out: Record<string, LiveRate> = {};
  for (const ref of refs) {
    const match = ref.r4ck;
    if (!match) continue;
    const hits = [...usable.values()].filter((o) => matchesGpu(o, match));
    if (hits.length === 0) continue;
    const prices = hits.map((o) => perGpuHourly(o) as number).sort((a, b) => a - b);
    out[ref.model] = {
      hourlyUsd: roundRate(median(prices)),
      lowUsd: roundRate(prices[0]),
      offers: hits.length,
      providers: new Set(hits.map((o) => o.provider?.slug ?? "?")).size,
    };
  }
  return out;
}

/** r4ck's `max_*` bounds are exclusive, so "exactly N" is [N, N + 0.5). */
export function cpuSearchParams(vcpu: number, ramGb: number): URLSearchParams {
  return new URLSearchParams({
    kind: "vps",
    min_vcpu: String(vcpu),
    max_vcpu: String(vcpu + 0.5),
    min_ram: String(ramGb - 0.25),
    max_ram: String(ramGb + 0.25),
    // Free plans are not a price.
    min_price: "0.01",
    sort: "price",
    order: "asc",
  });
}

/** Is a cached snapshot too old to use as is? */
export function isStale(rates: LiveRates | null, maxAgeMs: number, now = Date.now()): boolean {
  if (!rates) return true;
  const at = Date.parse(rates.fetchedAt);
  return !Number.isFinite(at) || now - at > maxAgeMs;
}
