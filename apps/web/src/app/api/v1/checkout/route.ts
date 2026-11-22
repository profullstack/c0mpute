import { checkout } from "@/lib/checkout";
import { json, sameOrigin } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * POST /api/v1/checkout: the /checkout form. Prices the selection on the
 * server, opens a top-up on Infernet for the signed-in person and answers 303
 * to the CoinPay payment page (or JSON when asked for it).
 *
 * @profullstack/checkout refuses a foreign Origin itself; sameOrigin() is the
 * same check every other cookie-authenticated write here makes.
 */
export async function POST(req: Request) {
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  return checkout().POST(req);
}
