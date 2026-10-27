/**
 * The c0mpute market fee: what c0mpute keeps from a job bought through the
 * market (c0mpute.com/market and the Infernet gateway). The node operator gets
 * the rest. Anthony set it on 2026-10-27; Infernet applies the same number as
 * its house fee on paid jobs.
 */
export const MARKET_FEE_PCT = 10;
export const OPERATOR_SHARE_PCT = 100 - MARKET_FEE_PCT;

/** Split a job price (decimal dollars) into the operator's share and the fee, in whole cents. */
export function splitPrice(usd: number): { operator: number; fee: number } {
  const cents = Math.round(usd * 100);
  const fee = Math.round((cents * MARKET_FEE_PCT) / 100);
  return { operator: (cents - fee) / 100, fee: fee / 100 };
}
