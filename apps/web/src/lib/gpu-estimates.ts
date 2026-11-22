import type { R4ckGpuMatch } from "./r4ck-rates";

/**
 * Hourly rental rates for one GPU, used as comparison inputs for the scenario
 * calculator. They are not c0mpute bids or observed c0mpute earnings.
 *
 * The live rate comes from r4ck.dev (see ./r4ck-rates.ts): the median per-GPU
 * price of the hourly offers r4ck lists for that card, refreshed daily. The
 * `hourlyUsd`/`source` below are the fallback used when r4ck is unreachable
 * or lists no offer for the card, checked by hand on GPU_RATE_CHECKED_AT:
 * - RunPod: the public GPU types API / rate card. Secure Cloud on-demand price
 *   when RunPod lists one, otherwise its Community Cloud price.
 * - Vast.ai: median on-demand base price ($/h, storage excluded) across
 *   rentable single-GPU offers, used for cards RunPod does not rent.
 * - Lambda: on-demand 1x price from its public rate card.
 * - Vast.ai lowest in-stock offer, read through r4ck.dev by hand.
 *
 * Cards with no public hourly rental market (AMD Radeon, Apple silicon) carry
 * `hourlyUsd: null` and no r4ck match: the page asks the operator for their
 * own comparison rate instead of inventing one.
 */
export const GPU_RATE_CHECKED_AT = "2026-11-22";

export const GPU_RATE_SOURCES = {
  "runpod-secure": { label: "RunPod Secure Cloud", url: "https://www.runpod.io/pricing" },
  "runpod-community": { label: "RunPod Community Cloud", url: "https://www.runpod.io/pricing" },
  "vast-median": { label: "Vast.ai median on-demand offer", url: "https://vast.ai/pricing" },
  lambda: { label: "Lambda on-demand", url: "https://lambda.ai/service/gpu-cloud/pricing" },
  "vast-low": { label: "Vast.ai lowest in-stock offer (via r4ck.dev)", url: "https://r4ck.dev/servers?gpu=1&interval=hour&sort=price" },
} as const;

export type GpuRateSourceId = keyof typeof GPU_RATE_SOURCES;

export type GpuGroup =
  | "NVIDIA GeForce"
  | "NVIDIA RTX workstation"
  | "AMD Radeon"
  | "Apple silicon"
  | "NVIDIA datacenter"
  | "AMD Instinct";

export const GPU_GROUPS: GpuGroup[] = [
  "NVIDIA GeForce",
  "NVIDIA RTX workstation",
  "AMD Radeon",
  "Apple silicon",
  "NVIDIA datacenter",
  "AMD Instinct",
];

export interface GpuRateReference {
  model: string;
  vram: string;
  group: GpuGroup;
  /** Hand-checked fallback rate in USD per hour, or null when there is none. */
  hourlyUsd: number | null;
  source: GpuRateSourceId | null;
  /** How r4ck names this card; null when r4ck has no market for it. */
  r4ck: R4ckGpuMatch | null;
}

const m = (pattern: RegExp, vram?: { min?: number; max?: number }): R4ckGpuMatch => ({
  pattern,
  ...(vram?.min != null ? { minVramGb: vram.min } : {}),
  ...(vram?.max != null ? { maxVramGb: vram.max } : {}),
});

export const GPU_RATE_REFERENCES: GpuRateReference[] = [
  // Consumer NVIDIA
  { model: "NVIDIA RTX 2080 Ti", vram: "11 GB", group: "NVIDIA GeForce", hourlyUsd: null, source: null, r4ck: m(/^rtx 2080 ti$/) },
  { model: "NVIDIA RTX 3060", vram: "12 GB", group: "NVIDIA GeForce", hourlyUsd: 0.05, source: "vast-low", r4ck: m(/^rtx 3060$/) },
  { model: "NVIDIA RTX 3060 Ti", vram: "8 GB", group: "NVIDIA GeForce", hourlyUsd: null, source: null, r4ck: m(/^rtx 3060 ti$/) },
  { model: "NVIDIA RTX 3070", vram: "8 GB", group: "NVIDIA GeForce", hourlyUsd: 0.13, source: "runpod-community", r4ck: m(/^rtx 3070$/) },
  { model: "NVIDIA RTX 3080", vram: "10 GB", group: "NVIDIA GeForce", hourlyUsd: 0.17, source: "runpod-community", r4ck: m(/^rtx 3080$/) },
  { model: "NVIDIA RTX 3090", vram: "24 GB", group: "NVIDIA GeForce", hourlyUsd: 0.11, source: "vast-low", r4ck: m(/^rtx 3090$/) },
  { model: "NVIDIA RTX 3090 Ti", vram: "24 GB", group: "NVIDIA GeForce", hourlyUsd: null, source: null, r4ck: m(/^rtx 3090 ti$/) },
  { model: "NVIDIA RTX 4060 Ti 16GB", vram: "16 GB", group: "NVIDIA GeForce", hourlyUsd: 0.16, source: "vast-median", r4ck: m(/^rtx 4060 ti$/, { min: 15 }) },
  { model: "NVIDIA RTX 4070", vram: "12 GB", group: "NVIDIA GeForce", hourlyUsd: 0.19, source: "vast-median", r4ck: m(/^rtx 4070$/) },
  { model: "NVIDIA RTX 4070 Super", vram: "12 GB", group: "NVIDIA GeForce", hourlyUsd: null, source: null, r4ck: m(/^rtx 4070s$/) },
  { model: "NVIDIA RTX 4070 Ti Super", vram: "16 GB", group: "NVIDIA GeForce", hourlyUsd: 0.21, source: "vast-median", r4ck: m(/^rtx (4070s ti|4070 ti s)$/) },
  { model: "NVIDIA RTX 4080", vram: "16 GB", group: "NVIDIA GeForce", hourlyUsd: 0.5, source: "runpod-secure", r4ck: m(/^rtx 4080$/) },
  { model: "NVIDIA RTX 4080 Super", vram: "16 GB", group: "NVIDIA GeForce", hourlyUsd: null, source: null, r4ck: m(/^rtx 4080s$/) },
  { model: "NVIDIA RTX 4090", vram: "24 GB", group: "NVIDIA GeForce", hourlyUsd: 0.28, source: "vast-low", r4ck: m(/^rtx 4090$/, { max: 30 }) },
  { model: "NVIDIA RTX 5060", vram: "8 GB", group: "NVIDIA GeForce", hourlyUsd: null, source: null, r4ck: m(/^rtx 5060$/) },
  { model: "NVIDIA RTX 5060 Ti", vram: "16 GB", group: "NVIDIA GeForce", hourlyUsd: null, source: null, r4ck: m(/^rtx 5060 ti$/) },
  { model: "NVIDIA RTX 5070", vram: "12 GB", group: "NVIDIA GeForce", hourlyUsd: null, source: null, r4ck: m(/^rtx 5070$/) },
  { model: "NVIDIA RTX 5070 Ti", vram: "16 GB", group: "NVIDIA GeForce", hourlyUsd: null, source: null, r4ck: m(/^rtx 5070 ti$/) },
  { model: "NVIDIA RTX 5080", vram: "16 GB", group: "NVIDIA GeForce", hourlyUsd: 0.59, source: "runpod-secure", r4ck: m(/^rtx 5080$/) },
  { model: "NVIDIA RTX 5090", vram: "32 GB", group: "NVIDIA GeForce", hourlyUsd: 1.19, source: "runpod-secure", r4ck: m(/^rtx 5090$/) },

  // Workstation NVIDIA
  { model: "NVIDIA RTX A4000", vram: "16 GB", group: "NVIDIA RTX workstation", hourlyUsd: null, source: null, r4ck: m(/^rtx a4000$/) },
  { model: "NVIDIA RTX A5000", vram: "24 GB", group: "NVIDIA RTX workstation", hourlyUsd: null, source: null, r4ck: m(/^rtx a5000$/) },
  { model: "NVIDIA RTX A6000", vram: "48 GB", group: "NVIDIA RTX workstation", hourlyUsd: null, source: null, r4ck: m(/^rtx a6000$/) },
  { model: "NVIDIA RTX 4000 Ada", vram: "20 GB", group: "NVIDIA RTX workstation", hourlyUsd: null, source: null, r4ck: m(/^rtx 4000 ada$/) },
  { model: "NVIDIA RTX 6000 Ada", vram: "48 GB", group: "NVIDIA RTX workstation", hourlyUsd: null, source: null, r4ck: m(/^rtx 6000 ada$/) },
  { model: "NVIDIA RTX PRO 5000 Blackwell", vram: "48 GB", group: "NVIDIA RTX workstation", hourlyUsd: null, source: null, r4ck: m(/^rtx pro 5000\b/) },
  { model: "NVIDIA RTX PRO 6000 Blackwell", vram: "96 GB", group: "NVIDIA RTX workstation", hourlyUsd: null, source: null, r4ck: m(/^(rtx pro 6000\b|rtx6000 blackwell$)/) },

  // AMD Radeon: no public hourly rental market for these cards.
  { model: "AMD Radeon RX 7900 XT", vram: "20 GB", group: "AMD Radeon", hourlyUsd: null, source: null, r4ck: null },
  { model: "AMD Radeon RX 7900 XTX", vram: "24 GB", group: "AMD Radeon", hourlyUsd: null, source: null, r4ck: null },

  // Apple silicon: unified memory (maximum configuration); no public hourly GPU rental market.
  { model: "Apple M2 Max", vram: "up to 96 GB unified", group: "Apple silicon", hourlyUsd: null, source: null, r4ck: null },
  { model: "Apple M2 Ultra", vram: "up to 192 GB unified", group: "Apple silicon", hourlyUsd: null, source: null, r4ck: null },
  { model: "Apple M3 Max", vram: "up to 128 GB unified", group: "Apple silicon", hourlyUsd: null, source: null, r4ck: null },
  { model: "Apple M3 Ultra", vram: "up to 512 GB unified", group: "Apple silicon", hourlyUsd: null, source: null, r4ck: null },
  { model: "Apple M4 Max", vram: "up to 128 GB unified", group: "Apple silicon", hourlyUsd: null, source: null, r4ck: null },

  // Datacenter NVIDIA
  { model: "NVIDIA T4", vram: "16 GB", group: "NVIDIA datacenter", hourlyUsd: 0.14, source: "vast-median", r4ck: m(/^t4$/) },
  { model: "NVIDIA V100", vram: "16-32 GB", group: "NVIDIA datacenter", hourlyUsd: null, source: null, r4ck: m(/^v100$/) },
  { model: "NVIDIA L4", vram: "24 GB", group: "NVIDIA datacenter", hourlyUsd: null, source: null, r4ck: m(/^l4$/) },
  { model: "NVIDIA A10", vram: "24 GB", group: "NVIDIA datacenter", hourlyUsd: 1.29, source: "lambda", r4ck: m(/^a10$/) },
  { model: "NVIDIA L40S", vram: "48 GB", group: "NVIDIA datacenter", hourlyUsd: 1.09, source: "runpod-secure", r4ck: m(/^l40s$/) },
  { model: "NVIDIA A100 PCIe 40GB", vram: "40 GB", group: "NVIDIA datacenter", hourlyUsd: null, source: null, r4ck: m(/^a100 pcie$/, { max: 50 }) },
  { model: "NVIDIA A100 SXM 40GB", vram: "40 GB", group: "NVIDIA datacenter", hourlyUsd: 1.0, source: "runpod-community", r4ck: m(/^a100 sxm4?$/, { max: 50 }) },
  { model: "NVIDIA A100 PCIe", vram: "80 GB", group: "NVIDIA datacenter", hourlyUsd: 1.79, source: "runpod-secure", r4ck: m(/^a100 pcie$/, { min: 60 }) },
  { model: "NVIDIA A100 SXM", vram: "80 GB", group: "NVIDIA datacenter", hourlyUsd: 1.79, source: "runpod-secure", r4ck: m(/^a100 sxm4?$/, { min: 60 }) },
  { model: "NVIDIA H100 PCIe", vram: "80 GB", group: "NVIDIA datacenter", hourlyUsd: 2.89, source: "runpod-secure", r4ck: m(/^h100 pcie$/) },
  { model: "NVIDIA H100 SXM", vram: "80 GB", group: "NVIDIA datacenter", hourlyUsd: 3.99, source: "runpod-secure", r4ck: m(/^h100 sxm5?$/) },
  { model: "NVIDIA H100 NVL", vram: "94 GB", group: "NVIDIA datacenter", hourlyUsd: null, source: null, r4ck: m(/^h100 nvl$/) },
  { model: "NVIDIA H200", vram: "141 GB", group: "NVIDIA datacenter", hourlyUsd: null, source: null, r4ck: m(/^h200( sxm)?$/) },
  { model: "NVIDIA H200 NVL", vram: "141 GB", group: "NVIDIA datacenter", hourlyUsd: null, source: null, r4ck: m(/^h200 nvl$/) },
  { model: "NVIDIA B200", vram: "180 GB", group: "NVIDIA datacenter", hourlyUsd: null, source: null, r4ck: m(/^b200( sxm)?$/) },
  { model: "NVIDIA B300", vram: "288 GB", group: "NVIDIA datacenter", hourlyUsd: null, source: null, r4ck: m(/^b300( sxm\d*)?( ac)?$/) },

  // AMD Instinct
  { model: "AMD Instinct MI300X", vram: "192 GB", group: "AMD Instinct", hourlyUsd: null, source: null, r4ck: m(/^mi300x$/) },
];

export const DEFAULT_GPU_MODEL = "NVIDIA RTX 4090";

export const UTILIZATION_SCENARIOS = [10, 25, 50] as const;
