// Paying relays for the live streams they carried (DIP-0019, per-GB).
//
// A relay's signed receipt reaches a status aggregator as a credited job of
// type "live.relay", priced at $0.01 per GB when the stream is on the paid
// tier and 0 on the free tier. The earnings recorder copies those jobs here;
// this module files each one against the stream it belongs to and charges the
// streamer's balance on Infernet, which credits the node (95%), the house
// (5%) and any referral commission out of the house fee. One receipt settles
// once: its job id is the idempotency key on both sides.
//
// No "server-only" import: the recorder loads this from instrumentation.ts.
import type { AggregatorJob } from "../earnings/store";
import { bytesForUsd, MAX_SPEND_USD_PER_HOUR, parseRelayJobId, type LiveStore } from "./store";

export const LIVE_RELAY_TYPE = "live.relay";
/** The most one receipt (60 s of one relay) may charge: 50 GB. */
export const MAX_RECEIPT_USD = 0.5;

export interface ChargeResult {
  charged: boolean;
  reason?: string;
}
/** Charge `subject`'s balance and credit the node. Throws when Infernet could not be reached. */
export type Charge = (subject: string, body: { idem_key: string; kind: string; cost_usd: number; provider: string; meta: Record<string, unknown> }) => Promise<ChargeResult>;

/** File the live relay receipts among a page of credited jobs. Returns how many were new. */
export async function fileReceipts(jobs: AggregatorJob[], store: LiveStore): Promise<number> {
  let filed = 0;
  for (const j of jobs) {
    if (j.type !== LIVE_RELAY_TYPE) continue;
    const id = parseRelayJobId(j.job_id);
    if (!id) continue;
    const stream = await store.byNetworkId(id.network_id);
    if (!stream) continue;
    const cost = Math.min(j.paid_usd, MAX_RECEIPT_USD);
    if (await store.fileUsage({ job_id: j.job_id, stream_id: stream.id, user_id: stream.user_id, node: j.node, bytes: bytesForUsd(cost), cost_usd: cost, at: j.completed_at_ms })) filed++;
  }
  return filed;
}

/**
 * Charge every filed receipt that has not been settled. A streamer with no
 * balance, no linked account or past the hourly cap is not charged: the
 * receipt closes unpaid and the stream goes to the free tier for its next
 * publish. A receipt Infernet could not be asked about stays open for the
 * next pass.
 */
export async function settlePass(store: LiveStore, charge: Charge, now = Date.now()): Promise<{ charged: number; unpaid: number; retry: number }> {
  const out = { charged: 0, unpaid: 0, retry: 0 };
  for (const u of await store.uncharged()) {
    const stream = await store.publicGet(u.stream_id);
    const subject = stream?.coinpay_sub ?? null;
    if (!subject || (await store.spentSince(u.user_id, now - 3_600_000)) + u.cost_usd > MAX_SPEND_USD_PER_HOUR) {
      await store.settle(u.job_id, 2);
      if (stream) await store.setPaid(stream.id, false);
      out.unpaid++;
      continue;
    }
    let res: ChargeResult;
    try {
      res = await charge(subject, { idem_key: u.job_id, kind: LIVE_RELAY_TYPE, cost_usd: u.cost_usd, provider: `c0mpute:${u.node}`, meta: { stream: u.stream_id, bytes: u.bytes } });
    } catch {
      out.retry++;
      continue;
    }
    if (res.charged) {
      await store.settle(u.job_id, 1);
      out.charged++;
    } else {
      await store.settle(u.job_id, 2);
      if (stream) await store.setPaid(stream.id, false);
      out.unpaid++;
    }
  }
  return out;
}
