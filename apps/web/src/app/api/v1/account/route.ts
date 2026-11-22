import { fleetFor, keepsAWayIn, signInMethods } from "@/lib/accounts/server";
import { balanceFor, currentUser, json, sameOrigin, store, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * GET /api/v1/account: everything the account page shows. The CoinPay
 * identity (DID), how this account signs in, its API keys, its nodes with
 * earnings, whether it can be paid, and its compute balance.
 */
export async function GET(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  const [methods, keys, fleet, balance, wallets] = await Promise.all([
    signInMethods(user),
    store().listKeys(user.id),
    fleetFor(user.id).catch(() => null),
    balanceFor(user.coinpay_sub),
    store().wallets(user.id),
  ]);
  return json({
    id: user.id,
    name: user.name,
    coinpay: user.coinpay_sub ? { connected: true, did: user.did } : { connected: false, did: null },
    sign_in: methods,
    keys,
    nodes: fleet ? { totals: fleet.totals, list: fleet.nodes } : null,
    payouts: user.coinpay_sub
      ? { ready: true, to: "your CoinPay account", wallets: wallets ?? [], manage: "https://coinpayportal.com" }
      : { ready: false, reason: "Connect CoinPay to get paid: node earnings and payouts go to your CoinPay wallet." },
    balance: balance
      ? {
          spendable_usd: balance.total_spendable_usd ?? "0.000000",
          earned_spendable_usd: balance.earned_spendable_usd ?? "0.000000",
          prepaid_available_usd: balance.prepaid_available_usd ?? "0.000000",
        }
      : null,
  });
}

/** PATCH /api/v1/account {name}: the display name. */
export async function PATCH(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const body = (await req.json().catch(() => ({}))) as { name?: unknown };
  if (typeof body.name !== "string") return json({ error: { message: "name is required", code: 400 } }, 400);
  await store().rename(user.id, body.name.trim() || null);
  return json({ ok: true });
}

/**
 * DELETE /api/v1/account?method=coinpay|password: remove a sign-in method,
 * as long as another remains. (Passkeys: /api/v1/account/passkeys.)
 */
export async function DELETE(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const method = new URL(req.url).searchParams.get("method");
  if (method !== "coinpay" && method !== "password") return json({ error: { message: "method: coinpay or password", code: 400 } }, 400);
  if (!(await keepsAWayIn(user, { [method]: true }))) {
    return json({ error: { message: "add a passkey first: this is your only way to sign in", code: 409 } }, 409);
  }
  if (method === "coinpay") await store().unlinkCoinpay(user.id);
  else await store().clearPassword(user.id);
  return json({ ok: true });
}
