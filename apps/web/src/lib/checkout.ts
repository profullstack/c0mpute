// /checkout: add compute credit. The page and the form handler come from
// @profullstack/checkout; the payment is a top-up opened on Infernet, where
// the balance lives (see lib/checkout-topup.ts).
import { createCheckout, type Checkout } from "@profullstack/checkout";
import { currentUser, infernet, linkInfernetAccount, siteUrl } from "./chat/server";
import { TOPUP_CUSTOM, TOPUP_OPTIONS, topUpAccount, topUpOnInfernet } from "./checkout-topup";

let _checkout: Checkout | null = null;

/**
 * Built on first use, not at module load: siteUrl() reads the environment, and
 * `next build` loads this module with none of it set.
 */
export const checkout = (): Checkout =>
  (_checkout ??= createCheckout({
    site: "c0mpute.com",
    title: "Add compute credit",
    subtitle: "Pay with any coin or PayPal through CoinPay, from $5.",
    action: "/api/v1/checkout",
    page: "/checkout",
    options: TOPUP_OPTIONS,
    custom: TOPUP_CUSTOM,
    requireAccount: true,
    signIn: "/signin",
    // Behind the proxy the Host header can be internal; the public origin is ours too.
    origins: [siteUrl()],
    account: async (request) => topUpAccount(request ? await currentUser(request) : null),
    pay: topUpOnInfernet({ infernet, link: linkInfernetAccount, siteUrl }),
  }));
