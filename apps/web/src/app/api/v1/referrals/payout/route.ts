import { bad } from "@/lib/chat/push-http";
import { currentUser, sameOrigin, unauthorized } from "@/lib/chat/server";
import { referralUpstream, relay } from "@/lib/referrals";

export const dynamic = "force-dynamic";

/**
 * POST /api/v1/referrals/payout { amount_usd? }  (empty = all available)
 * Withdraws confirmed referral commissions through the node payout path
 * (same minimum and auto-approve cap, sent by CoinPay).
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return bad("cross-site request refused", 403);
  const body = (await req.json().catch(() => ({}))) as { amount_usd?: unknown };
  const amount = body.amount_usd === undefined || body.amount_usd === null || body.amount_usd === "" ? undefined : Number(body.amount_usd);
  if (amount !== undefined && !(Number.isFinite(amount) && amount > 0)) return bad("amount_usd: a positive number, or leave it out for everything available");
  try {
    return relay(
      await referralUpstream(user.coinpay_sub)("/api/v1/referrals/payout", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(amount === undefined ? {} : { amount_usd: amount }),
        signal: AbortSignal.timeout(30_000),
      }),
    );
  } catch {
    return bad("payouts are unavailable right now", 503);
  }
}
