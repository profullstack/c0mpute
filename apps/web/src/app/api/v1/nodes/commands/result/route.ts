import { checkNodeRequest } from "@/lib/accounts/node-auth";
import { fleetService } from "@/lib/accounts/server";
import { clientIp, json, rateLimit, siteUrl } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** A node reports a command's outcome. Body {peer_id, ts, id, ok, output?, signature}. */
export async function POST(req: Request) {
  if (!rateLimit(`node-poll:${clientIp(req)}`, 3600).ok) return json({ error: { message: "slow down", code: 429 } }, 429);
  const body = (await req.json().catch(() => ({}))) as Record<string, unknown>;
  const problem = checkNodeRequest(siteUrl(), body, "result");
  if (problem) return json({ error: { message: problem, code: 401 } }, 401);
  const ok = await fleetService().report(body.peer_id as string, body.id as string, body.ok as boolean, body.output);
  return ok ? json({ ok: true }) : json({ error: { message: "no such command for this node", code: 404 } }, 404);
}
