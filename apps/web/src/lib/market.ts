/**
 * The c0mpute market fee: what c0mpute keeps from a job bought through the
 * market (c0mpute.com/market and the Infernet gateway). The node operator gets
 * the rest. Anthony set it on 2026-10-27; Infernet applies the same number as
 * its house fee on paid jobs.
 */
export const MARKET_FEE_PCT = 5;
export const OPERATOR_SHARE_PCT = 100 - MARKET_FEE_PCT;

/** Split a job price (decimal dollars) into the operator's share and the fee, in whole cents. */
export function splitPrice(usd: number): { operator: number; fee: number } {
  const cents = Math.round(usd * 100);
  const fee = Math.round((cents * MARKET_FEE_PCT) / 100);
  return { operator: (cents - fee) / 100, fee: fee / 100 };
}

/**
 * Share of the buyer's cap a dynamic node bids before it has seen an auction.
 * After that it prices off the clearing price it learns: it undercuts a winning
 * bid it lost to by UNDERCUT, probes higher after a win, rises toward the cap as
 * its queue fills, and never goes under the operator's minimum. The lowest bid
 * wins. Mirrors node/crates/c0mpute-core/src/pricing.rs.
 */
export const DYNAMIC_IDLE_SHARE = 0.6;
export const UNDERCUT = 0.02;
