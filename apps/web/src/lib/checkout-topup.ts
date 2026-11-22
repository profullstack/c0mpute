// The pure half of /checkout: what is for sale and how a top-up is opened on
// Infernet. Nothing here imports lib/chat/server.ts (which pulls in the SQLite
// Cloud driver), so it can be unit-tested with a mocked upstream;
// lib/checkout.ts wires it to the real one.
import type { Account, CheckoutConfig, Order } from "@profullstack/checkout";
import { unwrapData } from "./chat/envelope";

/** `infernet(path, init, { subject })` from lib/chat/server.ts. */
export type InfernetCall = (path: string, init?: RequestInit, opts?: { subject?: string | null }) => Promise<Response>;

export type TopUpDeps = {
  infernet: InfernetCall;
  /** linkInfernetAccount from lib/chat/server.ts: gives the CoinPay sub an Infernet account. */
  link: (info: { sub: string; name?: string | null }) => Promise<unknown>;
  /** The canonical public origin (no trailing slash). Infernet only accepts return URLs on it. */
  siteUrl: () => string;
  warn?: (message: string) => void;
};

export const TOPUP_PATH = "/api/v1/billing/topup";

/**
 * "about N chats" is the amount divided by what an average chat costs on the
 * house model ($0.005; Infernet lib/billing/core.js). The page says so once.
 */
export const TOPUP_OPTIONS: NonNullable<CheckoutConfig["options"]> = [
  { id: "5", name: "$5", price: 5, gets: "about 1,000 chats" },
  { id: "10", name: "$10", price: 10, gets: "about 2,000 chats" },
  { id: "25", name: "$25", price: 25, popular: true, gets: "about 5,000 chats" },
  { id: "100", name: "$100", price: 100, gets: "about 20,000 chats" },
];

/** Infernet's own limits for one top-up. */
export const TOPUP_CUSTOM = { min: 5, max: 10000, default: 50 };

/** The link parameters /checkout keeps when it sends someone to sign in. */
export const LINK_PARAMS = ["option", "plan", "amount", "code", "source"] as const;

/**
 * Who pays: the CoinPay sub, because that is how Infernet knows the person.
 * A passkey or password-only account has none, so it has nothing to top up.
 */
export function topUpAccount(user: { coinpay_sub?: string | null; name?: string | null } | null | undefined): Account | null {
  return user?.coinpay_sub ? { id: user.coinpay_sub, email: "", name: user.name ?? null } : null;
}

/** `/checkout` plus the short, known link parameters (safeNext caps `next` at 512 chars). */
export function checkoutPath(query: Record<string, string | string[] | undefined>): string {
  const qs = new URLSearchParams();
  for (const key of LINK_PARAMS) {
    const raw = query[key];
    const value = Array.isArray(raw) ? raw[0] : raw;
    if (value) qs.set(key, value.slice(0, 64));
  }
  return qs.size ? `/checkout?${qs}` : "/checkout";
}

/** The error string of an Infernet reply ({ error: "code" } or { error: { code, message } }), safe to log. */
async function errorOf(res: Response): Promise<string> {
  let code = "";
  try {
    const body = (await res.json()) as { error?: unknown } | null;
    const e = body?.error;
    if (typeof e === "string") code = e;
    else if (e && typeof e === "object") {
      const o = e as { code?: unknown; type?: unknown; message?: unknown };
      code = [o.code, o.type, o.message].find((v): v is string => typeof v === "string") ?? "";
    }
  } catch {
    // not JSON: the status is all there is
  }
  return code.replace(/[^\w .:-]/g, "").slice(0, 80);
}

/**
 * pay(order) for @profullstack/checkout: opens a CoinPay payment for a compute
 * top-up through Infernet, as the trusted client, for the signed-in person.
 * Anything but a 201 with a checkout URL throws, and the package then shows
 * "We could not open the payment page. Nothing was charged; try again."
 */
export function topUpOnInfernet(deps: TopUpDeps) {
  const warn = deps.warn ?? ((m: string) => console.warn(m));
  return async function pay(order: Order): Promise<{ url: string; id?: string }> {
    const subject = order.account?.id;
    if (!subject) throw new Error("top-up needs a CoinPay account");
    const origin = deps.siteUrl();
    const post = () =>
      deps.infernet(
        TOPUP_PATH,
        {
          method: "POST",
          headers: { "content-type": "application/json", accept: "application/json" },
          body: JSON.stringify({
            amount_usd: order.totalCents / 100,
            success_url: `${origin}/checkout/thanks`,
            cancel_url: `${origin}/checkout`,
          }),
          signal: AbortSignal.timeout(20_000),
        },
        { subject },
      );

    let res = await post();
    let error = res.status === 201 ? "" : await errorOf(res);
    if (res.status === 404 && error === "subject_not_linked") {
      // Signed in before accounts were linked at sign-in, or the link failed
      // then: make the Infernet account now and try once more.
      const name = typeof order.account.name === "string" ? order.account.name : null;
      await deps.link({ sub: subject, name });
      res = await post();
      error = res.status === 201 ? "" : await errorOf(res);
    }
    if (res.status !== 201) {
      warn(`infernet top-up failed: HTTP ${res.status}${error ? ` ${error}` : ""}`);
      throw new Error(`infernet top-up HTTP ${res.status}${error ? ` ${error}` : ""}`);
    }

    const data = unwrapData(await res.json().catch(() => null));
    const topup = data?.topup && typeof data.topup === "object" ? (data.topup as Record<string, unknown>) : null;
    const url = [data?.checkout_url, topup?.checkout_url].find((v): v is string => typeof v === "string" && /^https:\/\//.test(v));
    if (!url) {
      warn("infernet top-up failed: 201 without a checkout_url");
      throw new Error("infernet top-up returned no checkout_url");
    }
    return { url, ...(typeof topup?.id === "string" ? { id: topup.id } : {}) };
  };
}
