/**
 * DigitalOcean Basic shared-CPU Droplet rates used only as cloud-VM comparison
 * inputs for the c0mpute scenarios. They include RAM, storage, and transfer;
 * they are not c0mpute bids or observed CPU-worker earnings.
 */
export const CPU_RATE_SOURCE = {
  label: "DigitalOcean Basic shared-CPU Droplets",
  url: "https://www.digitalocean.com/pricing/droplets",
} as const;

export const CPU_RATE_CHECKED_AT = "2026-11-10";

export interface CpuRateReference {
  model: string;
  specs: string;
  hourlyUsd: number;
}

export const CPU_RATE_REFERENCES: CpuRateReference[] = [
  { model: "1 vCPU / 1 GiB", specs: "1 shared vCPU · 1 GiB RAM", hourlyUsd: 0.00893 },
  { model: "2 vCPUs / 4 GiB", specs: "2 shared vCPUs · 4 GiB RAM", hourlyUsd: 0.03571 },
  { model: "4 vCPUs / 8 GiB", specs: "4 shared vCPUs · 8 GiB RAM", hourlyUsd: 0.07143 },
  { model: "8 vCPUs / 16 GiB", specs: "8 shared vCPUs · 16 GiB RAM", hourlyUsd: 0.14286 },
];

export const DEFAULT_CPU_MODEL = CPU_RATE_REFERENCES[0].model;
