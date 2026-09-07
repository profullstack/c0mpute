import Link from "next/link";

export const metadata = {
  title: "pricing — c0mpute",
  description: "c0mpute uses a peer-to-peer market rate model. Buyers set a max price per job; providers quote. The open protocol takes 0% — providers earn the full amount. Optional managed services are priced separately.",
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
          c0mpute has no fixed pricing. You set a maximum price per job via{" "}
          <code>--max-price</code>. Providers advertise indicative rates and
          submit binding offers.{" "}
          <strong className="text-[var(--color-fg)]">
            Your c0mpute client — or a gateway you chose — selects an eligible
            offer
          </strong>{" "}
          according to your price, latency, trust and validation policy. There
          is no matcher in the middle deciding on your behalf.
        </p>
        <p>
          An offer is only eligible if it is for your job, within your cap,
          and able to finish before your deadline. Everything past that is
          policy you pick: cheapest, fastest, balanced, trusted, private.
        </p>
        <p>
          Payment releases from escrow when the result passes the validation
          level your job asked for, and both sides sign a receipt. Settlement
          runs through the adapter your job names —{" "}
          <a href="https://coinpayportal.com" target="_blank" rel="noopener noreferrer">
            CoinPay
          </a>{" "}
          by default, and it is not the only option.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ what c0mpute charges ]</h2>
        <table className="w-full text-left border-collapse text-xs">
          <tbody className="divide-y divide-[var(--color-rule)]">
            <tr>
              <td className="py-2 pr-6 align-top accent whitespace-nowrap">protocol</td>
              <td className="py-2 text-[var(--color-dim)] leading-6">
                <strong className="text-[var(--color-fg)]">0% mandatory platform fee.</strong>{" "}
                The amount agreed is the amount transferred. Running a node,
                advertising capacity, discovering providers and settling a job
                cost nothing beyond the price of the work.
              </td>
            </tr>
            <tr>
              <td className="py-2 pr-6 align-top accent whitespace-nowrap">managed services</td>
              <td className="py-2 text-[var(--color-dim)] leading-6">
                Priced separately, when you choose to use them — API gateway
                usage, fiat and card billing, enterprise invoicing, SLA
                routing, reserved capacity, observability, support. Explicit
                service fees, never a cut taken out of the market.
              </td>
            </tr>
          </tbody>
        </table>
        <p className="text-xs text-[var(--color-dim)]">
          The split is architectural, not just billing policy: hosted services
          are clients of the public protocol and cannot be required for two
          peers to transact. → <Link href="/protocol">protocol</Link>
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
        <h2 className="font-semibold text-[var(--color-fg)]">[ pricing units ]</h2>
        <p>
          A job is not the only billable unit. Providers quote in whichever
          unit fits the workload, and the protocol carries a unit without
          needing to know what it means — so a plugin can define its own.
        </p>
        <p className="text-xs text-[var(--color-dim)] leading-6">
          <code>job</code> · <code>second</code> · <code>cpu-core-second</code>{" "}
          · <code>gpu-second</code> · <code>gpu-memory-gib-second</code> ·{" "}
          <code>1m-input-tokens</code> · <code>1m-output-tokens</code> ·{" "}
          <code>image</code> · <code>video-minute</code> ·{" "}
          <code>audio-minute</code> · <code>gb-month</code> ·{" "}
          <code>gb-transferred</code> · <code>request</code> ·{" "}
          <code>reserved-capacity-hour</code>
        </p>
        <p className="text-xs text-[var(--color-dim)]">
          Amounts are decimal strings, never floating point — two nodes must
          agree on a price to the byte for a signature over it to verify.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ for providers ]</h2>
        <p>
          Register hardware as a c0mpute provider and earn by completing jobs.
          You set your own rates and you keep everything you earn. Payment
          releases automatically when output passes validation.
        </p>
        <p className="text-[var(--color-dim)]">
          Your earning record is portable: reputation is derived from signed
          receipts you hold, not from a score in a database we control.
        </p>
        <p className="text-xs text-[var(--color-dim)]">
          → <Link href="/providers">run a provider</Link>
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
