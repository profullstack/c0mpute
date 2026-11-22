import { accountStore, fleetService } from "@/lib/accounts/server";
import { currentUser, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** GET /api/v1/fleet/audit[?limit=100]: who did what to which node, and the commands sent with their outcome. */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const limit = Number(new URL(req.url).searchParams.get("limit")) || 100;
  const [log, commands] = await Promise.all([accountStore().auditLog(user.id, limit), fleetService().commands(user.id, limit)]);
  return json({ log, commands });
}
