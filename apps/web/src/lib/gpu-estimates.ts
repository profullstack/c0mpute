/**
 * Public hourly rental rates for one GPU, used as comparison inputs for the
 * scenario calculator. They are not c0mpute bids or observed c0mpute earnings.
 *
 * Each row names where its rate came from:
 * - RunPod: the public GPU types API / rate card. Secure Cloud on-demand price
 *   when RunPod lists one, otherwise its Community Cloud price.
 * - Vast.ai: median on-demand base price ($/h, storage excluded) across
 *   rentable single-GPU offers, used for cards RunPod does not rent.
 * - Lambda: on-demand 1x price from its public rate card.
 *
 * Cards with no public hourly rental market (AMD Radeon, Apple silicon) carry
 * `hourlyUsd: null`: the page asks the operator for their own comparison rate
 * instead of inventing one. Update when the public rate cards change.
 */
export const GPU_RATE_CHECKED_AT = "2026-11-08";

export const GPU_RATE_SOURCES = {
  "runpod-secure": { label: "RunPod Secure Cloud", url: "https://www.runpod.io/pricing" },
  "runpod-community": { label: "RunPod Community Cloud", url: "https://www.runpod.io/pricing" },
  "vast-median": { label: "Vast.ai median on-demand offer", url: "https://vast.ai/pricing" },
  lambda: { label: "Lambda on-demand", url: "https://lambda.ai/service/gpu-cloud/pricing" },
} as const;

export type GpuRateSourceId = keyof typeof GPU_RATE_SOURCES;

export type GpuGroup = "NVIDIA GeForce" | "AMD Radeon" | "Apple silicon" | "NVIDIA datacenter";

export const GPU_GROUPS: GpuGroup[] = ["NVIDIA GeForce", "AMD Radeon", "Apple silicon", "NVIDIA datacenter"];

export interface GpuRateReference {
  model: string;
  vram: string;
  group: GpuGroup;
  /** Public comparison rate in USD per hour, or null when no public rental market exists. */
  hourlyUsd: number | null;
  source: GpuRateSourceId | null;
}

export const GPU_RATE_REFERENCES: GpuRateReference[] = [
  // Consumer NVIDIA
  { model: "NVIDIA RTX 3060", vram: "12 GB", group: "NVIDIA GeForce", hourlyUsd: 0.07, source: "vast-median" },
  { model: "NVIDIA RTX 3070", vram: "8 GB", group: "NVIDIA GeForce", hourlyUsd: 0.13, source: "runpod-community" },
  { model: "NVIDIA RTX 3080", vram: "10 GB", group: "NVIDIA GeForce", hourlyUsd: 0.17, source: "runpod-community" },
  { model: "NVIDIA RTX 3090", vram: "24 GB", group: "NVIDIA GeForce", hourlyUsd: 0.5, source: "runpod-secure" },
  { model: "NVIDIA RTX 4060 Ti 16GB", vram: "16 GB", group: "NVIDIA GeForce", hourlyUsd: 0.16, source: "vast-median" },
  { model: "NVIDIA RTX 4070", vram: "12 GB", group: "NVIDIA GeForce", hourlyUsd: 0.19, source: "vast-median" },
  { model: "NVIDIA RTX 4070 Ti Super", vram: "16 GB", group: "NVIDIA GeForce", hourlyUsd: 0.21, source: "vast-median" },
  { model: "NVIDIA RTX 4080", vram: "16 GB", group: "NVIDIA GeForce", hourlyUsd: 0.5, source: "runpod-secure" },
  { model: "NVIDIA RTX 4090", vram: "24 GB", group: "NVIDIA GeForce", hourlyUsd: 0.89, source: "runpod-secure" },
  { model: "NVIDIA RTX 5080", vram: "16 GB", group: "NVIDIA GeForce", hourlyUsd: 0.59, source: "runpod-secure" },
  { model: "NVIDIA RTX 5090", vram: "32 GB", group: "NVIDIA GeForce", hourlyUsd: 1.19, source: "runpod-secure" },

  // AMD: no public hourly rental market for these cards.
  { model: "AMD Radeon RX 7900 XT", vram: "20 GB", group: "AMD Radeon", hourlyUsd: null, source: null },
  { model: "AMD Radeon RX 7900 XTX", vram: "24 GB", group: "AMD Radeon", hourlyUsd: null, source: null },

  // Apple silicon: unified memory (maximum configuration); no public hourly GPU rental market.
  { model: "Apple M2 Max", vram: "up to 96 GB unified", group: "Apple silicon", hourlyUsd: null, source: null },
  { model: "Apple M2 Ultra", vram: "up to 192 GB unified", group: "Apple silicon", hourlyUsd: null, source: null },
  { model: "Apple M3 Max", vram: "up to 128 GB unified", group: "Apple silicon", hourlyUsd: null, source: null },
  { model: "Apple M3 Ultra", vram: "up to 512 GB unified", group: "Apple silicon", hourlyUsd: null, source: null },
  { model: "Apple M4 Max", vram: "up to 128 GB unified", group: "Apple silicon", hourlyUsd: null, source: null },

  // Datacenter NVIDIA
  { model: "NVIDIA T4", vram: "16 GB", group: "NVIDIA datacenter", hourlyUsd: 0.14, source: "vast-median" },
  { model: "NVIDIA A10", vram: "24 GB", group: "NVIDIA datacenter", hourlyUsd: 1.29, source: "lambda" },
  { model: "NVIDIA L40S", vram: "48 GB", group: "NVIDIA datacenter", hourlyUsd: 1.09, source: "runpod-secure" },
  { model: "NVIDIA A100 SXM 40GB", vram: "40 GB", group: "NVIDIA datacenter", hourlyUsd: 1.0, source: "runpod-community" },
  { model: "NVIDIA A100 PCIe", vram: "80 GB", group: "NVIDIA datacenter", hourlyUsd: 1.79, source: "runpod-secure" },
  { model: "NVIDIA A100 SXM", vram: "80 GB", group: "NVIDIA datacenter", hourlyUsd: 1.79, source: "runpod-secure" },
  { model: "NVIDIA H100 PCIe", vram: "80 GB", group: "NVIDIA datacenter", hourlyUsd: 2.89, source: "runpod-secure" },
  { model: "NVIDIA H100 SXM", vram: "80 GB", group: "NVIDIA datacenter", hourlyUsd: 3.99, source: "runpod-secure" },
];

export const DEFAULT_GPU_MODEL = "NVIDIA RTX 4090";

export const UTILIZATION_SCENARIOS = [10, 25, 50] as const;
