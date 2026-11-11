// Shared bits for the /api/v1/hosting/* routes.
import { PushError, type HostingNode, type Push, type PushStore } from "./push";
import { json } from "./server";

export function pushErr(e: unknown) {
  if (e instanceof PushError) return json({ error: { message: e.message, code: e.status } }, e.status);
  console.error("hosting push", e);
  return json({ error: { message: "push store unavailable; try again", code: 503 } }, 503);
}

export const bad = (message: string, code = 400) => json({ error: { message, code } }, code);

/**
 * Which node a push goes to: the one named, else the node that signed the
 * site, else the account's own (most recently seen), else the c0mpute cloud
 * node. Paid pushes (`--disks`) need a node that pays its hosts through CoinPay.
 */
export async function pickNode(ps: PushStore, userId: string, name: unknown, existing?: Push | null): Promise<HostingNode | Response> {
  const r = await ps.pickNode(userId, name, existing);
  return "error" in r ? bad(r.error, r.code) : r;
}
