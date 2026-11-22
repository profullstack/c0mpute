import "@profullstack/checkout/checkout.css";
import { Checkout } from "@profullstack/checkout/react";
import { headers } from "next/headers";
import { redirect } from "next/navigation";
import { balanceFor, currentUser, siteUrl } from "@/lib/chat/server";
import { checkout } from "@/lib/checkout";
import { checkoutPath } from "@/lib/checkout-topup";

export const metadata = {
  title: "Add compute credit",
  description: "Add compute credit to your c0mpute balance with any coin or PayPal through CoinPay, from $5.",
  robots: { index: false },
};

export const dynamic = "force-dynamic";

type Query = Record<string, string | string[] | undefined>;

/**
 * The signed-in person, read on the server: the form has to render here and
 * work without JavaScript. Only the session cookie is passed on.
 */
async function signedIn() {
  const h = await headers();
  return currentUser(new Request(`${siteUrl()}/checkout`, { headers: { cookie: h.get("cookie") ?? "" } }));
}

function usd(value: unknown): string | null {
  const n = Number(value);
  if (value == null || value === "" || !Number.isFinite(n)) return null;
  return `$${n > 0 && n < 0.01 ? n.toFixed(6) : n.toFixed(2)}`;
}

export default async function CheckoutPage({ searchParams }: { searchParams: Promise<Query> }) {
  const query = await searchParams;
  const user = await signedIn();
  if (!user) redirect(`/signin?next=${encodeURIComponent(checkoutPath(query))}`);

  if (!user.coinpay_sub) {
    return (
      <div className="max-w-xl mx-auto px-4 sm:px-6 py-12 space-y-6 text-sm leading-7">
        <header className="space-y-2">
          <h1 className="text-2xl font-bold accent">Add compute credit</h1>
          <p className="comment">// connect CoinPay first</p>
        </header>
        <p>
          Your compute balance is kept under your CoinPay identity, and this account signs in with a passkey or password only, so there is
          nothing to add credit to yet. Connect CoinPay once and come back here; chat keeps working on the free, rate-limited tier in the
          meantime.
        </p>
        <p>
          <a className="inline-block !border border-[var(--color-accent)] px-4 py-2 accent" href="/api/auth/coinpay/login?link=1&next=/checkout">
            Connect CoinPay
          </a>
        </p>
        <p className="text-[var(--color-dim)]">
          <a href="/chat">Back to chat</a>
        </p>
      </div>
    );
  }

  const balance = await balanceFor(user.coinpay_sub);
  const spendable = usd(balance?.total_spendable_usd ?? balance?.spendable_usd);

  return (
    <div className="c0-checkout max-w-xl mx-auto px-4 sm:px-6 py-12 space-y-6 text-sm leading-7">
      {spendable && (
        <p className="text-center text-[var(--color-dim)]">
          current balance <span className="accent">{spendable}</span>
        </p>
      )}
      <Checkout view={checkout().view({ query, account: { email: "" } })} />
      <ul className="space-y-2 border-t border-[var(--color-rule)] pt-4 text-xs leading-6 text-[var(--color-dim)] max-w-sm mx-auto list-none">
        <li>
          Chat counts are an estimate from an average chat on the house model (about $0.005). What a message really costs depends on the
          model and how long it is.
        </li>
        <li>Your balance also includes what your own nodes have earned.</li>
        <li>With an empty balance, chat still works on the free, rate-limited tier.</li>
        <li>
          <a href="/chat">Back to chat</a>
        </li>
      </ul>
    </div>
  );
}
