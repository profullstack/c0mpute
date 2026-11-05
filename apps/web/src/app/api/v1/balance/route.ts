import { balanceFor, currentUser, json, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/balance — the signed-in person's compute balance.
 *
 * c0mpute chat runs on the Infernet network, and the balance lives there:
 * money topped up (CoinPay or x402) plus what this person's nodes have earned
 * and not withdrawn. Inference draws it down; when it is empty the network
 * serves the free, rate-limited tier instead of refusing.
 *
 * Read through our trusted client key, naming the person by their CoinPay
 * subject — the same identity they signed in here with.
 */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();

  const balance = await balanceFor(user.coinpay_sub);
  if (!balance) {
    // Infernet unreachable, or no Infernet account linked to this identity
    // yet. Either way there is nothing to spend, and chat still works free.
    return json(
      {
        linked: false,
        total_spendable_usd: "0.000000",
        note: "no compute balance yet; chat runs on the free tier",
        top_up: "https://infernetprotocol.com/dashboard/billing",
      },
      200,
      { "cache-control": "no-store" },
    );
  }
  return json(
    { ...balance, top_up: "https://infernetprotocol.com/dashboard/billing" },
    200,
    { "cache-control": "no-store" },
  );
}
