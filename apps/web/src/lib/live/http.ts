// Shared bits of the /api/v1/live routes.
import { timingSafeEqual } from "node:crypto";
import { balanceFor, bearer, currentUser, env, infernet, json, rateLimit } from "../chat/server";
import type { Charge } from "./settle";
import { FREE_MAX_HEIGHT, FREE_MAX_VIEWERS, LIVE_SCOPE, LiveError, MAX_SPEND_USD_PER_HOUR, type LiveStore } from "./store";

/** A session, a full API key, or an OAuth token carrying live:publish. */
export const liveUser = (req: Request) => currentUser(req, { scope: LIVE_SCOPE });

export const liveRate = (userId: string) => rateLimit(`live:${userId}`, 600);

export function liveUnauthorized() {
  return json(
    { error: { code: "unauthorized", message: `Send Authorization: Bearer <token>: a c0mpute.com OAuth token with the ${LIVE_SCOPE} scope, or an API key from https://c0mpute.com/account.` } },
    401,
    { "www-authenticate": `Bearer realm="c0mpute", scope="${LIVE_SCOPE}"` },
  );
}

export function liveErr(e: unknown) {
  if (e instanceof LiveError) return json({ error: { code: e.status === 409 ? "conflict" : "bad_request", message: e.message } }, e.status);
  console.error("live api", e);
  return json({ error: { code: "server_error", message: "live streaming is unavailable right now" } }, 503);
}

/** Relays are paid only once settlement is switched on for this deployment. */
export const settlementOn = () => env("C0MPUTE_LIVE_SETTLE") === "1";

/** The spendable balance in USD, or 0 when there is none or it cannot be read. */
export async function spendableUsd(subject: string | null | undefined): Promise<number> {
  const b = (await balanceFor(subject)) as Record<string, unknown> | null;
  const v = Number(b?.total_spendable_usd ?? b?.spendable_usd ?? 0);
  return Number.isFinite(v) && v > 0 ? v : 0;
}

/**
 * Paid tier: settlement is on, the owner has a balance, and they are under
 * the hourly spend cap. Otherwise the free tier (capped, relays unpaid).
 */
export async function tierFor(store: LiveStore, user: { id: string; coinpay_sub: string | null }, now = Date.now()) {
  const paid = settlementOn() && (await spendableUsd(user.coinpay_sub)) > 0 && (await store.spentSince(user.id, now - 3_600_000)) < MAX_SPEND_USD_PER_HOUR;
  return { paid, max_height: paid ? null : FREE_MAX_HEIGHT, max_viewers: paid ? null : FREE_MAX_VIEWERS };
}

/** The ingest node's shared secret (C0MPUTE_LIVE_INGEST_SECRET), compared in constant time. */
export function ingestAuthorized(req: Request): boolean {
  const want = env("C0MPUTE_LIVE_INGEST_SECRET");
  const got = bearer(req) ?? "";
  if (want.length < 24 || got.length !== want.length) return false;
  return timingSafeEqual(Buffer.from(got), Buffer.from(want));
}

/** Charge the streamer on Infernet and credit the relay there. Throws when Infernet did not answer. */
export const liveCharge: Charge = async (subject, body) => {
  const r = await infernet(
    "/api/v1/billing/usage-charge",
    { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body), signal: AbortSignal.timeout(15_000) },
    { subject },
  );
  if (!r.ok) throw new Error(`infernet usage-charge: ${r.status}`);
  const d = (await r.json()) as Record<string, unknown>;
  const data = (d.data && typeof d.data === "object" ? d.data : d) as Record<string, unknown>;
  return { charged: data.charged === true, reason: typeof data.reason === "string" ? data.reason : undefined };
};
