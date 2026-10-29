import type { Metadata } from "next";
import Link from "next/link";
import { EarningsEstimator } from "@/components/earnings-estimator";
import { getStatusPayload } from "@/lib/status";

export const metadata: Metadata = {
  title: "GPU earnings estimates — c0mpute",
  description: "Estimate illustrative GPU earnings scenarios and see the current c0mpute network demand before you run a node.",
  alternates: { canonical: "https://c0mpute.com/estimates" },
};

export const dynamic = "force-dynamic";

export default async function EstimatesPage() {
  const status = await getStatusPayload();
  const jobsCompleted24h = status.source === "aggregator" ? status.network.jobs_completed_24h : null;
  const online = status.source === "aggregator" ? status.network.workers_online : null;

  return (
    <div className="max-w-3xl mx-auto px-6 py-10 sm:py-14 space-y-7">
      <header className="space-y-3">
        <p className="comment">// choose a GPU · model possible earning scenarios</p>
        <h1 className="text-2xl sm:text-3xl font-bold accent">GPU earnings estimates</h1>
        <p className="text-sm leading-6 text-[var(--color-fg)]">
          Pick a GPU from the list or type your own. Compare what it could gross at different paid-use levels, after the c0mpute market fee.
        </p>
        <p className="text-xs text-[var(--color-dim)]">
          Live network: {online === null ? "data unavailable" : `${online.toLocaleString()} nodes online`} · {jobsCompleted24h === null ? "earnings activity unavailable" : `${jobsCompleted24h.toLocaleString()} jobs completed in the last 24 hours`}.
        </p>
      </header>

      <EarningsEstimator jobsCompleted24h={jobsCompleted24h} />

      <p className="text-xs text-[var(--color-dim)]">
        Ready to join? <Link href="/providers">Run a node</Link> · <Link href="/market">See current providers</Link> · <Link href="/pricing">How pricing works</Link>
      </p>
    </div>
  );
}
