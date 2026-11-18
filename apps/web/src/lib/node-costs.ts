// Power and fee arithmetic for node earnings, free of server-only imports so
// unit tests and the operator report can use it.
import { OPERATOR_SHARE_PCT } from "./market";

/**
 * Rough power draw while a job runs, by board class. Only used for the
 * "est. power" column: the board knows the class, not the machine, so this is
 * a typical figure for a desktop under load, not a measurement.
 */
export const BUSY_WATTS: Record<string, number> = {
  cpu: 65,
  "gpu:apple": 40,
  "gpu:nvidia": 250,
  "gpu:amd": 250,
};

/** Electricity price used for the estimate (about the US residential average). */
export const POWER_USD_PER_KWH = 0.17;

/** Estimated electricity spent on `busySeconds` of work on a node of `cls`. */
export function powerCostUsd(cls: string, busySeconds: number): number {
  const watts = BUSY_WATTS[cls] ?? BUSY_WATTS.cpu;
  return (watts / 1000) * (busySeconds / 3600) * POWER_USD_PER_KWH;
}

/** What the operator keeps from what buyers paid: the share after the house fee. */
export function operatorEarnings(paidUsd: number): number {
  return (paidUsd * OPERATOR_SHARE_PCT) / 100;
}
