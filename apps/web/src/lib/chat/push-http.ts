// Shared bits for the /api/v1/hosting/* routes.
import { PushError, type HostingNode, type PushStore } from "./push";
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
  const nodes = await ps.nodes(userId);
  if (!nodes.length) {
    return bad(
      "no hosting node linked to this account yet. Run on the machine that should publish: c0mpute chat login && c0mpute chat connect hosting --name <node>",
      409,
    );
  }
  let node: HostingNode | undefined;
  if (typeof name === "string" && name) {
    node = nodes.find((n) => n.name === name.toLowerCase() || n.id === name);
    if (!node) return bad(`no node named ${name} on this account (yours: ${nodes.map((n) => n.name).join(", ")})`, 404);
  } else {
    node = [...nodes].sort((a, b) => String(b.last_seen_at ?? "").localeCompare(String(a.last_seen_at ?? "")))[0];
  }
  if (node.payments !== "coinpay") {
    return json(
      {
        error: {
          message: `node ${node.name} cannot pay hosts: enable CoinPay payments on it ([payments] enabled = true, rail = "coinpay", the API key in its env), then restart c0mpute chat connect hosting`,
          code: 402,
        },
      },
      402,
    );
  }
  return node;
}
