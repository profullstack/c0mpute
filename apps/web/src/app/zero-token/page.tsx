import Link from "next/link";

export const metadata = {
  title: "zero token — c0mpute",
  description: "c0mpute has no native token. Payments are handled via coinpayportal.com — accept crypto without holding or speculating on a project token.",
  alternates: { canonical: "https://c0mpute.com/zero-token" },
};

export default function ZeroTokenPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">zero token</h1>
        <p className="comment">// no native token. no speculation. just compute.</p>
      </header>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ what zero token means ]</h2>
        <p>
          c0mpute does not have a native token. There is no <code>$C0M</code>,
          no governance token, no presale, no vesting schedule. You don't need
          to buy or hold anything to use the network.
        </p>
        <p>
          Most compute networks require you to acquire their token before you
          can pay for a job. That creates friction, speculation risk, and
          gatekeeping. We skip all of that.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ how payments work ]</h2>
        <p>
          Payments on c0mpute are handled via{" "}
          <a href="https://coinpayportal.com" target="_blank" rel="noopener noreferrer">
            coinpayportal.com
          </a>
          {" "}— a crypto payment gateway that supports BTC, ETH, LTC, USDC, and
          more. Buyers pay in whichever coin they already hold. Workers receive
          payment directly — no platform token required on either side.
        </p>
        <ul className="space-y-1 mt-3 list-none">
          <li><span className="accent">buyers</span> — pay with existing crypto. No new token to acquire.</li>
          <li><span className="accent">workers</span> — receive payment in the coin agreed at job time.</li>
          <li><span className="accent">escrow</span> — funds are held in escrow until the job completes successfully.</li>
          <li><span className="accent">no middleman</span> — coinpayportal settles directly between parties.</li>
        </ul>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ why not a token ]</h2>
        <p>
          Project tokens align incentives on paper but introduce real problems
          in practice: liquidity risk, regulatory exposure, and a community
          that's focused on price instead of product. We'd rather build
          something people actually use.
        </p>
        <p>
          If governance or staking ever makes sense down the road, we'll
          revisit. For now, the protocol is the product.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ get started ]</h2>
        <ul className="space-y-2">
          <li>
            <Link href="/getting-started" className="hover:text-[var(--color-accent)]">getting-started</Link>{" "}
            <span className="text-[var(--color-dim)]">— install the CLI and run your first job</span>
          </li>
          <li>
            <Link href="/pricing" className="hover:text-[var(--color-accent)]">pricing</Link>{" "}
            <span className="text-[var(--color-dim)]">— how job costs are calculated</span>
          </li>
          <li>
            <a href="https://coinpayportal.com" target="_blank" rel="noopener noreferrer">
              coinpayportal.com
            </a>{" "}
            <span className="text-[var(--color-dim)]">— payment gateway powering c0mpute transactions</span>
          </li>
        </ul>
      </section>
    </div>
  );
}
