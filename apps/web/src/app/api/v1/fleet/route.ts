import { fleetFor, fleetService } from "@/lib/accounts/server";
import { currentUser, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/fleet[?tag=rack-a]: every node on the account with status
 * (online, last seen), version, hardware, jobs and earnings (24h / 30d / all,
 * buyer-agreed before the fee), tags, verification and queued commands, plus
 * totals. Same data as `c0mpute fleet ls --json` and the MCP tool fleet_list.
 */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const tag = new URL(req.url).searchParams.get("tag")?.toLowerCase() || null;
  try {
    const f = await fleetFor(user.id);
    const nodes = tag ? f.nodes.filter((n) => n.tags.includes(tag)) : f.nodes;
    return json({ nodes, totals: f.totals, fleet_key: await fleetService().publicKey(), payouts_ready: !!user.coinpay_sub });
  } catch (e) {
    console.error("fleet", e);
    return json({ error: { message: "could not read your fleet right now", code: 503 } }, 503);
  }
}
