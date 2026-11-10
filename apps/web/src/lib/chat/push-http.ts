// Shared bits for the /api/v1/hosting/* routes.
import { chooseNode, PushError, type HostingNode, type PushStore } from "./push";
import { json } from "./server";

export function pushErr(e: unknown) {
  if (e instanceof PushError) return json({ error: { message: e.message, code: e.status } }, e.status);
  console.error("hosting push", e);
  return json({ error: { message: "push store unavailable; try again", code: 503 } }, 503);
}

export const bad = (message: string, code = 400) => json({ error: { message, code } }, code);

/**
 * Which node a push goes to: the one named, else the only one, else the one
 * seen most recently. Paid pushes (`--disks`) need a node that pays its hosts
 * through CoinPay.
 */
export async function pickNode(ps: PushStore, userId: string, name: unknown): Promise<HostingNode | Response> {
  const r = chooseNode(await ps.nodes(userId), name);
  return "error" in r ? bad(r.error, r.code) : r;
}
