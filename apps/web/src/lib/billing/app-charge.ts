/**
 * An approved app charges the signed-in person's compute balance for work it
 * did itself (POST /api/v1/billing/charge; OAuth scope "billing").
 *
 * The app holds an OAuth key carrying "billing", which only clients listed in
 * C0MPUTE_BILLING_CLIENTS can get (oauth/authorize refuses everyone else). The
 * charge goes to Infernet's usage-charge as the house kind for that app
 * (Board as a Service: "board.meeting"), billed to the person's CoinPay
 * subject, all or nothing. c0mpute never takes a subject from the app: it is
 * always the key's own person.
 *
 * Idempotent on the app's idem_key, scoped to the person, so a retried charge
 * is never a second charge.
 */
import { BILLING_KINDS } from "../chat/oauth";

export const MAX_APP_CHARGE_USD = 50;

export type AppChargeInput = { idemKey: string; kind: string; costUsd: number; meta: Record<string, unknown> | null };
export type InfernetCharge = (
  subject: string,
  body: { idem_key: string; kind: string; cost_usd: number; provider: string; meta: Record<string, unknown> },
) => Promise<{ status: number; data: Record<string, unknown> }>;

export function validateAppCharge(body: unknown): { ok: true; value: AppChargeInput } | { ok: false; error: string } {
  const b = (body && typeof body === "object" && !Array.isArray(body) ? body : null) as Record<string, unknown> | null;
  if (!b) return { ok: false, error: "a JSON object is required" };
  const idemKey = typeof b.idem_key === "string" ? b.idem_key.trim() : "";
  if (!idemKey || idemKey.length > 120 || !/^[\x21-\x7e]+$/.test(idemKey)) return { ok: false, error: "idem_key: 1 to 120 printable characters, no spaces" };
  if (!(BILLING_KINDS as readonly string[]).includes(String(b.kind))) return { ok: false, error: `kind must be one of: ${BILLING_KINDS.join(", ")}` };
  const cost = b.cost_usd;
  if (typeof cost !== "number" || !Number.isFinite(cost) || cost <= 0) return { ok: false, error: "cost_usd must be a positive number" };
  if (cost > MAX_APP_CHARGE_USD) return { ok: false, error: `cost_usd is over the ${MAX_APP_CHARGE_USD} USD limit for one charge` };
  if (cost < 0.000001) return { ok: false, error: "cost_usd is under one micro-dollar" };
  let meta: Record<string, unknown> | null = null;
  if (b.meta != null) {
    if (typeof b.meta !== "object" || Array.isArray(b.meta)) return { ok: false, error: "meta must be an object" };
    const text = JSON.stringify(b.meta);
    if (text.length > 1500) return { ok: false, error: "meta is over 1500 characters" };
    meta = JSON.parse(text);
  }
  return { ok: true, value: { idemKey, kind: String(b.kind), costUsd: cost, meta } };
}

export type AppChargeResult =
  | { status: 200; body: { charged: true; cost_usd: number; balance_usd: number } }
  | { status: 402; body: { charged: false; reason: string; balance_usd: number | null; top_up_url: string } }
  | { status: number; body: { error: { code: string; message: string } } };

/** Charge `user` for one app action. Pure apart from `infernet`. */
export async function chargeForApp(opts: {
  user: { id: string; coinpay_sub: string | null };
  client: string;
  input: AppChargeInput;
  infernet: InfernetCharge;
  topUpUrl: string;
}): Promise<AppChargeResult> {
  const { user, input, topUpUrl } = opts;
  if (!user.coinpay_sub) {
    return { status: 402, body: { charged: false, reason: "no_account", balance_usd: null, top_up_url: topUpUrl } };
  }
  let r: { status: number; data: Record<string, unknown> };
  try {
    r = await opts.infernet(user.coinpay_sub, {
      // Infernet scopes keys per trusted client (c0mpute); scope them per person and app here.
      idem_key: `app:${opts.client}:${user.id}:${input.idemKey}`.slice(0, 200),
      kind: input.kind,
      cost_usd: input.costUsd,
      provider: "house",
      meta: { ...(input.meta ?? {}), app: opts.client, c0mpute_user: user.id },
    });
  } catch {
    return { status: 503, body: { error: { code: "unavailable", message: "billing did not answer; retry with the same idem_key" } } };
  }
  if (r.status === 409) return { status: 409, body: { error: { code: "idem_key_reused", message: "that idem_key was already used for a different charge" } } };
  if (r.status >= 400) return { status: r.status >= 500 ? 503 : r.status, body: { error: { code: "charge_refused", message: String(r.data.error ?? `billing answered ${r.status}`) } } };
  const d = (r.data.data && typeof r.data.data === "object" ? r.data.data : r.data) as Record<string, unknown>;
  const balance = Number(d.balance_usd);
  if (d.charged === true) return { status: 200, body: { charged: true, cost_usd: Number(d.cost_usd), balance_usd: balance } };
  return {
    status: 402,
    body: { charged: false, reason: String(d.reason ?? "insufficient_balance"), balance_usd: Number.isFinite(balance) ? balance : null, top_up_url: topUpUrl },
  };
}
