/**
 * Snapshot of public RunPod Community Cloud hourly rates, checked 2026-10-29.
 * These are comparison inputs for the scenario calculator, not c0mpute bids
 * or observed c0mpute earnings. Update when the public rate card changes.
 */
export const GPU_RATE_SOURCE = "https://www.runpod.io/pricing";
export const GPU_RATE_CHECKED_AT = "2026-10-29";

export interface GpuRateReference {
  model: string;
  vram: string;
  hourlyUsd: number;
}

export const GPU_RATE_REFERENCES: GpuRateReference[] = [
  { model: "NVIDIA RTX 3090", vram: "24 GB", hourlyUsd: 0.5 },
  { model: "NVIDIA RTX 4090", vram: "24 GB", hourlyUsd: 0.89 },
  { model: "NVIDIA RTX 5090", vram: "32 GB", hourlyUsd: 1.19 },
  { model: "NVIDIA L40S", vram: "48 GB", hourlyUsd: 1.09 },
  { model: "NVIDIA A100 PCIe", vram: "80 GB", hourlyUsd: 1.79 },
  { model: "NVIDIA A100 SXM", vram: "80 GB", hourlyUsd: 1.79 },
  { model: "NVIDIA H100 PCIe", vram: "80 GB", hourlyUsd: 2.89 },
  { model: "NVIDIA H100 SXM", vram: "80 GB", hourlyUsd: 3.99 },
];

export const UTILIZATION_SCENARIOS = [10, 25, 50] as const;
