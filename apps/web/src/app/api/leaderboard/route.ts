import { NextResponse } from "next/server";
import { getLeaderboard } from "@/lib/leaderboard/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/leaderboard: the anonymous network leaderboard as JSON (the data
 * behind /leaderboard). Rebuilt at most every 5 minutes. Nobody is named:
 * public node ids and operator nicknames (as on /nodes) and "anon-xxxxxx"
 * handles only; boards with fewer than 3 people are withheld. See
 * lib/leaderboard/anon.ts for the rules.
 */
export async function GET() {
  const payload = await getLeaderboard();
  if (!payload) {
    return NextResponse.json({ error: "leaderboard is rebuilding; try again in a minute" }, { status: 503, headers: { "retry-after": "60" } });
  }
  return NextResponse.json(payload, {
    headers: {
      "cache-control": "public, max-age=60, s-maxage=300",
      "access-control-allow-origin": "*",
    },
  });
}
