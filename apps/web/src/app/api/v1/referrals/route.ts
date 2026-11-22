import { bad } from "@/lib/chat/push-http";
import { currentUser, sameOrigin, unauthorized } from "@/lib/chat/server";
import { forwardedIp, referralUpstream, relay } from "@/lib/referrals";

export const dynamic = "force-dynamic";

/**
 * GET  /api/v1/referrals  your node referral code, commission term, referred
 *                         nodes, commissions (pending / confirmed / paid)
 * POST /api/v1/referrals  create your code (once; the term is set then)
 *
 * Proxied to Infernet (where c0mpute jobs settle) for the signed-in person,
 * by their CoinPay sub. One code works on both sites.
 */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  try {
    return relay(await referralUpstream(user.coinpay_sub)("/api/v1/referrals", { signal: AbortSignal.timeout(10_000), cache: "no-store" }));
  } catch {
    return bad("referrals are unavailable right now", 503);
  }
}

export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return bad("cross-site request refused", 403);
  try {
    return relay(
      await referralUpstream(user.coinpay_sub, forwardedIp(req))("/api/v1/referrals", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: "{}",
        signal: AbortSignal.timeout(10_000),
      }),
    );
  } catch {
    return bad("referrals are unavailable right now", 503);
  }
}
