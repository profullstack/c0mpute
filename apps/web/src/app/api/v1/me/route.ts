import { balanceFor, currentUser, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  // What this person can spend on inference: money they topped up plus what
  // their nodes earned. Null when Infernet is unreachable or no account is
  // linked yet, which is not an error - chat then runs on the free tier.
  const balance = await balanceFor(user.coinpay_sub);
  return json({
    id: user.id,
    did: user.did,
    name: user.name,
    auth: user.token_kind,
    balance: balance
      ? {
          spendable_usd: balance.total_spendable_usd ?? "0.000000",
          earned_spendable_usd: balance.earned_spendable_usd ?? "0.000000",
          prepaid_available_usd: balance.prepaid_available_usd ?? "0.000000",
        }
      : null,
  });
}
