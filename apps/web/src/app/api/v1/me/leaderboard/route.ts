import { currentUser, json, sameOrigin, unauthorized } from "@/lib/chat/server";
import { leaderboardStore } from "@/lib/leaderboard/server";

export const dynamic = "force-dynamic";

const err = (status: number, message: string) => json({ error: { message, type: "c0mpute_error", code: status } }, status);

/** GET: whether this account is left off the public /leaderboard. */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  return json({ hidden: await leaderboardStore().isOptedOut(user.id) });
}

/**
 * POST { hidden: boolean }: leave this account off /leaderboard (or come back).
 * Hides the account's pseudonymous rows (users, API, agents, affiliates, and
 * Infernet nodes linked to it) and the c0mpute nodes it claimed from the node
 * itself. Takes effect on the next rebuild (at most 5 minutes).
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return err(403, "cross-site request refused");
  const body = (await req.json().catch(() => null)) as { hidden?: unknown } | null;
  if (typeof body?.hidden !== "boolean") return err(400, "send { hidden: true } or { hidden: false }");
  await leaderboardStore().setOptOut(user.id, body.hidden);
  return json({ hidden: body.hidden });
}
