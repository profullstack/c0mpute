import { checkNodeRequest } from "@/lib/accounts/node-auth";
import { cleanNodeState } from "@/lib/accounts/fleet";
import { fleetService } from "@/lib/accounts/server";
import { clientIp, json, rateLimit, siteUrl } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * A linked node fetches its fleet commands (pause, resume, update, restart,
 * and the settings models, house-models, uncensored, hosting, storage), and
 * may send its state (a JSON string: settings, models served, disk, a failed
 * update), kept for the fleet view and the account's alerts; then the signed
 * text ends with "\nstate: <sha256 hex of the state string>".
 * Body {peer_id, ts, signature, state?}: the node signs
 * "c0mpute node poll v1\nsite: <site>\nnode: <peer_id>\nts: <ts>" with its
 * identity key. Each command comes signed by this site's fleet key; the node
 * checks that signature, its own peer id and the account it linked to.
 */
export async function POST(req: Request) {
  if (!rateLimit(`node-poll:${clientIp(req)}`, 3600).ok) return json({ error: { message: "slow down", code: 429 } }, 429);
  const body = (await req.json().catch(() => ({}))) as Record<string, unknown>;
  const problem = checkNodeRequest(siteUrl(), body, "poll");
  if (problem) return json({ error: { message: problem, code: 401 } }, 401);
  if (typeof body.state === "string") {
    let parsed: unknown = null;
    try {
      parsed = JSON.parse(body.state);
    } catch {}
    const st = cleanNodeState(parsed);
    if (st) await fleetService().saveState(body.peer_id as string, st).catch((e) => console.error("fleet: could not save node state", e));
  }
  const r = await fleetService().forNode(body.peer_id as string);
  return json({ ...r, fleet_key: await fleetService().publicKey(), poll_seconds: 60 });
}
