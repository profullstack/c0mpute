import Link from "next/link";

export const metadata = {
  title: "pricing — c0mpute",
  description: "c0mpute uses a peer-to-peer market rate model. Buyers set a max price per job. Workers earn the full amount — no platform cut. Payments via CoinPay escrow.",
  alternates: { canonical: "https://c0mpute.com/pricing" },
};

export default function PricingPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">pricing</h1>
        <p className="comment">// peer-to-peer market rates, no platform cut</p>
      </header>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ how it works ]</h2>
        <p>
          c0mpute has no fixed pricing. Buyers set a maximum price per job
          via the <code>--max-price</code> flag. Workers advertise their
          rates. The network matches them. If a worker quotes under your cap,
          the job runs and payment is released from escrow on completion.
        </p>
        <p>
          There is no platform fee. Workers earn 100% of what buyers pay.
          Payments are settled via{" "}
          <a href="https://coinpayportal.com" target="_blank" rel="noopener noreferrer">
            CoinPay
          </a>{" "}
          escrow using your DID identity.
        </p>
      </section>

      <section className="space-y-4 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ example job costs ]</h2>
        <table className="w-full text-left border-collapse">
          <thead>
            <tr className="border-b border-[var(--color-rule)] text-[var(--color-dim)]">
              <th className="pb-2 pr-6 font-medium">job type</th>
              <th className="pb-2 pr-6 font-medium">example flag</th>
              <th className="pb-2 font-medium">typical range</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-[var(--color-rule)]">
            <tr>
              <td className="py-2 pr-6">HLS transcode (1080p)</td>
              <td className="py-2 pr-6 font-mono text-xs">--max-price 1.25</td>
              <td className="py-2 text-[var(--color-dim)]">$0.50 – $1.25</td>
            </tr>
            <tr>
              <td className="py-2 pr-6">4K AV1 transcode</td>
              <td className="py-2 pr-6 font-mono text-xs">--max-price 5.00</td>
              <td className="py-2 text-[var(--color-dim)]">$2.00 – $5.00</td>
            </tr>
            <tr>
              <td className="py-2 pr-6">LLM inference (per run)</td>
              <td className="py-2 pr-6 font-mono text-xs">--max-price 0.10</td>
              <td className="py-2 text-[var(--color-dim)]">$0.01 – $0.10</td>
            </tr>
          </tbody>
        </table>
        <p className="text-[var(--color-dim)] text-xs">
          Rates are set by workers and fluctuate with supply. These are
          illustrative examples from early testing — not guaranteed rates.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ for workers ]</h2>
        <p>
          Register your GPU hardware as a c0mpute worker and earn by
          completing jobs. You set your own rate. You keep everything you
          earn. Workers are paid in USD-equivalent via CoinPay escrow,
          released automatically when job output passes validation.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ free to start ]</h2>
        <p>
          The c0mpute CLI is free and open source (MIT). Installing and
          running the CLI, creating a DID, and exploring the network costs
          nothing. You only pay when you submit a job.
        </p>
        <p>
          <Link href="/getting-started" className="hover:text-[var(--color-accent)]">
            Install the CLI →
          </Link>
        </p>
      </section>
    </div>
  );
}
