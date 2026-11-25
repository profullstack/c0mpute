import { chargeForApp, validateAppCharge } from "@/lib/billing/app-charge";
import { currentUser, infernet, json, rateLimit, siteUrl, tokenScopes } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/**
 * POST /api/v1/billing/charge: an approved app charges the signed-in person's
 * compute balance for work it did itself.
 *
 *   Authorization: Bearer c0k_...   an OAuth key carrying the "billing" scope
 *   { idem_key, kind: "board.meeting", cost_usd, meta? }
 *
 *   200 { charged: true, cost_usd, balance_usd }
 *   402 { charged: false, reason: "insufficient_balance" | "no_account", balance_usd, top_up_url }
 *
 * Only OAuth keys: never a browser session (a cross-site form must not be able
 * to spend a balance) and never a full API key (it has no app to name). Only
 * apps in C0MPUTE_BILLING_CLIENTS can get the scope at all (oauth/authorize).
 */
export async function POST(req: Request) {
  const user = await currentUser(req, { scope: "billing" });
  if (!user || user.token_kind !== "oauth" || !tokenScopes(user).includes("billing")) {
    return json({ error: { code: "unauthorized", message: "an OAuth key with the billing scope is required" } }, 401, { "www-authenticate": 'Bearer realm="c0mpute", scope="billing"' });
  }
  if (!rateLimit(`billing-charge:${user.id}`, 120).ok) return json({ error: { code: "slow_down", message: "too many charges this hour" } }, 429);
  const parsed = validateAppCharge(await req.json().catch(() => null));
  if (!parsed.ok) return json({ error: { code: "bad_request", message: parsed.error } }, 400);
  const out = await chargeForApp({
    user: { id: user.id, coinpay_sub: user.coinpay_sub ?? null },
    client: (user.token_label ?? "app").replace(/ \(OAuth\)$/, "").replace(/[^\x21-\x7e]/g, "-").slice(0, 40),
    input: parsed.value,
    topUpUrl: `${siteUrl()}/checkout`,
    infernet: async (subject, body) => {
      const r = await infernet(
        "/api/v1/billing/usage-charge",
        { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body), signal: AbortSignal.timeout(15_000) },
        { subject },
      );
      return { status: r.status, data: ((await r.json().catch(() => ({}))) ?? {}) as Record<string, unknown> };
    },
  });
  return json(out.body, out.status);
}
