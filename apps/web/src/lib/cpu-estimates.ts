/**
 * CPU cloud-VM references for the c0mpute scenarios. The live rate is the
 * median monthly price (over 730 hours) of VPS plans r4ck.dev lists with
 * exactly that many vCPUs and that much RAM (see ./r4ck-rates.ts). The
 * `hourlyUsd` below is the fallback when r4ck is unreachable: DigitalOcean
 * Basic shared-CPU Droplet rates, checked by hand on CPU_RATE_CHECKED_AT.
 * VM prices include RAM, storage, and transfer; they are not c0mpute bids or
 * observed CPU-worker earnings.
 */
export const CPU_RATE_SOURCE = {
  label: "DigitalOcean Basic shared-CPU Droplets",
  url: "https://www.digitalocean.com/pricing/droplets",
} as const;

export const CPU_RATE_CHECKED_AT = "2026-11-10";

export interface CpuRateReference {
  model: string;
  specs: string;
  vcpu: number;
  ramGb: number;
  /** Fallback rate (DigitalOcean), USD per hour. */
  hourlyUsd: number;
}

export const CPU_RATE_REFERENCES: CpuRateReference[] = [
  { model: "1 vCPU / 1 GiB", specs: "1 shared vCPU · 1 GiB RAM", vcpu: 1, ramGb: 1, hourlyUsd: 0.00893 },
  { model: "2 vCPUs / 4 GiB", specs: "2 shared vCPUs · 4 GiB RAM", vcpu: 2, ramGb: 4, hourlyUsd: 0.03571 },
  { model: "4 vCPUs / 8 GiB", specs: "4 shared vCPUs · 8 GiB RAM", vcpu: 4, ramGb: 8, hourlyUsd: 0.07143 },
  { model: "8 vCPUs / 16 GiB", specs: "8 shared vCPUs · 16 GiB RAM", vcpu: 8, ramGb: 16, hourlyUsd: 0.14286 },
];

export const DEFAULT_CPU_MODEL = CPU_RATE_REFERENCES[0].model;
