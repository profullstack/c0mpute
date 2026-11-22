import { bad } from "@/lib/chat/push-http";
import { currentUser, sameOrigin, unauthorized } from "@/lib/chat/server";
import { referralUpstream, relay } from "@/lib/referrals";

export const dynamic = "force-dynamic";

/** POST /api/v1/referrals/address { coin, network, address }: where referral commission payouts go. */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return bad("cross-site request refused", 403);
  const b = (await req.json().catch(() => ({}))) as Record<string, unknown>;
  const pick = (v: unknown) => (typeof v === "string" ? v.trim().slice(0, 128) : "");
  try {
    return relay(
      await referralUpstream(user.coinpay_sub)("/api/v1/referrals/address", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ coin: pick(b.coin), network: pick(b.network), address: pick(b.address) }),
        signal: AbortSignal.timeout(10_000),
      }),
    );
  } catch {
    return bad("referrals are unavailable right now", 503);
  }
}
