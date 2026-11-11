import { listRuns } from "@/lib/chat/runs";
import { currentUser, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/chat/runs: this account's resumable turns (running, and finished in the last 15 minutes). */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  return json({ data: listRuns(user.id) });
}
