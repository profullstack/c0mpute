import { dashboardFor } from "@/lib/earnings/server";
import { parseRange } from "@/lib/earnings/report";
import { currentUser, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/operator/earnings?range=1h|3h|24h|7d|30d
 *
 * The signed-in operator's nodes: earned (after the house fee) vs spend vs net,
 * per time bucket, per node and per job type. Spend is the running cost the
 * operator set for a node, else estimated electricity for its busy time.
 */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const range = parseRange(new URL(req.url).searchParams.get("range"));
  try {
    return json(await dashboardFor(user, range));
  } catch (e) {
    console.error("operator earnings failed", e);
    return json({ error: { message: "could not read node earnings right now", code: 503 } }, 503);
  }
}
