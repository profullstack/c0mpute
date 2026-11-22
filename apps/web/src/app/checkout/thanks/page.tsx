import { BalanceNow } from "./balance-now";

export const metadata = {
  title: "Payment received",
  description: "Your compute credit top-up is on its way to your c0mpute balance.",
  robots: { index: false },
};

export const dynamic = "force-dynamic";

/** Where CoinPay sends the buyer back after a top-up (success_url). */
export default function CheckoutThanksPage() {
  return (
    <div className="max-w-xl mx-auto px-4 sm:px-6 py-12 space-y-6 text-sm leading-7">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">Payment received</h1>
        <p className="comment">// thank you</p>
      </header>
      <p>
        A PayPal payment is credited right away. A crypto payment is credited once the network confirms it, usually within a minute or
        two. This page checks your balance for a couple of minutes; you do not need to keep it open.
      </p>
      <BalanceNow />
      <p className="flex flex-wrap gap-x-6 gap-y-2">
        <a href="/chat">Back to chat</a>
        <a href="/checkout">Add more credit</a>
      </p>
    </div>
  );
}
