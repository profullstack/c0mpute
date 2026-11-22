import { fleetFor } from "@/lib/accounts/server";
import { balanceFor, currentUser, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

export async function GET(req: Request) {
  // OAuth keys with the "inference" scope may read who they are and the balance they spend.
  const user = await currentUser(req, { scope: "inference" });
  if (!user) return unauthorized();
  // What this person can spend on inference: money they topped up plus what
  // their nodes earned. Null when Infernet is unreachable or no account is
  // linked yet, which is not an error - chat then runs on the free tier.
  // An app-held (OAuth) key never sees the fleet.
  const nodesWanted = new URL(req.url).searchParams.get("nodes") === "1" && user.token_kind !== "oauth";
  const [balance, fleet] = await Promise.all([balanceFor(user.coinpay_sub), nodesWanted ? fleetFor(user.id).catch(() => null) : Promise.resolve(null)]);
  return json({
    id: user.id,
    did: user.did,
    name: user.name,
    auth: user.token_kind,
    coinpay: !!user.coinpay_sub,
    payouts_ready: !!user.coinpay_sub,
    adult_verified_at: user.adult_verified_at ?? null,
    balance: balance
      ? {
          spendable_usd: balance.total_spendable_usd ?? "0.000000",
          earned_spendable_usd: balance.earned_spendable_usd ?? "0.000000",
          prepaid_available_usd: balance.prepaid_available_usd ?? "0.000000",
        }
      : null,
    // ?nodes=1: the linked nodes too (what `c0mpute whoami` and the TUI show).
    ...(fleet ? { nodes: fleet.nodes.map((n) => ({ node: n.node, label: n.label, online: n.online, verified: n.verified, tags: n.tags, version: n.version })), totals: fleet.totals } : {}),
  });
}
