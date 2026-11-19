import { currentUser, json, unauthorized } from "@/lib/chat/server";
import { parseRange } from "@/lib/earnings/report";
import { diskDashboardFor } from "@/lib/disk/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/disk?range=1h|3h|24h|7d|30d
 *
 * Disk (hosting) money for the signed-in account, both sides:
 * - sites: your hosted sites (pushed from c0mpute chat, or claimed by site id
 *   or owner key) with size, copies, price, holders and what they cost;
 * - nodes: your nodes' paid bytes held and what they earned holding others' sites;
 * - totals and buckets: earned vs spend vs net, accrued by the settlement rule.
 */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const range = parseRange(new URL(req.url).searchParams.get("range"));
  try {
    return json(await diskDashboardFor(user, range));
  } catch (e) {
    console.error("disk dashboard failed", e);
    return json({ error: { message: "could not read disk earnings and spend right now", code: 503 } }, 503);
  }
}
