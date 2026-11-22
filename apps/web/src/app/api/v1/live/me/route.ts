import { json, liveStore } from "@/lib/chat/server";
import { liveErr, liveUnauthorized, liveUser, spendableUsd, tierFor } from "@/lib/live/http";
import { FREE_MAX_HEIGHT, FREE_MAX_VIEWERS, LIVE_RTMP_URL, LIVE_USD_PER_GB } from "@/lib/live/store";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/live/me (live:publish): who a connected app is publishing as,
 * and which tier their streams run on. The one account read this scope gets.
 */
export async function GET(req: Request) {
  const user = await liveUser(req);
  if (!user) return liveUnauthorized();
  try {
    const tier = await tierFor(liveStore(), user);
    return json({
      account: { id: user.id, label: user.name || user.did || user.id },
      balance_usd: await spendableUsd(user.coinpay_sub),
      tier: tier.paid ? "paid" : "free",
      usd_per_gb: LIVE_USD_PER_GB,
      free_tier: { max_height: FREE_MAX_HEIGHT, max_viewers: FREE_MAX_VIEWERS },
      rtmp_url: LIVE_RTMP_URL,
      top_up: "https://infernetprotocol.com/dashboard/billing",
    });
  } catch (e) {
    return liveErr(e);
  }
}
