// Experts store knobs. Prices and the trial length are store policy, not code:
// each has a default here and an env override (read at request time, so a
// change is an app.env edit + restart, no deploy).

const num = (name: string, fallback: number, env: Record<string, string | undefined>): number => {
  const v = Number(env[name]);
  return env[name] !== undefined && env[name] !== "" && Number.isFinite(v) && v >= 0 ? v : fallback;
};

export type ExpertsConfig = {
  /** Free days per CoinPay account, once, covering every expert. */
  trialDays: number;
  /** One expert, per period, USD. When unset the manifest's list price is used. */
  priceUsd: number | null;
  /** Every expert, per period, USD. */
  bundlePriceUsd: number;
  /** A paid period, days. */
  periodDays: number;
  /** Expert questions per account per hour (on top of the chat rate limit). */
  asksPerHour: number;
};

export const EXPERTS_DEFAULTS = {
  trialDays: 90,
  bundlePriceUsd: 29,
  periodDays: 30,
  asksPerHour: 60,
} as const;

export function expertsConfig(env: Record<string, string | undefined> = process.env): ExpertsConfig {
  const per = env.C0MPUTE_EXPERTS_PRICE_USD;
  return {
    trialDays: Math.floor(num("C0MPUTE_EXPERTS_TRIAL_DAYS", EXPERTS_DEFAULTS.trialDays, env)),
    priceUsd: per !== undefined && per !== "" && Number.isFinite(Number(per)) && Number(per) >= 0 ? Number(per) : null,
    bundlePriceUsd: num("C0MPUTE_EXPERTS_BUNDLE_PRICE_USD", EXPERTS_DEFAULTS.bundlePriceUsd, env),
    periodDays: Math.max(1, Math.floor(num("C0MPUTE_EXPERTS_PERIOD_DAYS", EXPERTS_DEFAULTS.periodDays, env))),
    asksPerHour: Math.max(1, Math.floor(num("C0MPUTE_EXPERTS_ASKS_PER_HOUR", EXPERTS_DEFAULTS.asksPerHour, env))),
  };
}

/** The price of one expert: the store override, else the publisher's list price. */
export function expertPriceUsd(listPriceUsd: number, cfg: ExpertsConfig): number {
  return cfg.priceUsd ?? listPriceUsd;
}
