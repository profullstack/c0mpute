import type { Metadata } from "next";
import Link from "next/link";
import { EarningsEstimator } from "@/components/earnings-estimator";
import { getStatusPayload } from "@/lib/status";

export const metadata: Metadata = {
  title: "Compute earnings estimates — c0mpute",
  description: "Compare illustrative CPU and GPU rental-price scenarios with live c0mpute network activity before you run a node.",
  alternates: { canonical: "https://c0mpute.com/estimates" },
};

export const dynamic = "force-dynamic";

export default async function EstimatesPage() {
  const status = await getStatusPayload();
  const jobsCompleted24h = status.source === "aggregator" ? status.network.jobs_completed_24h : null;
  const online = status.source === "aggregator" ? status.network.workers_online : null;
  const cpuWorkersOnline = status.source === "aggregator"
    ? status.network.workers_with_tag["c0mpute:cpu"] ?? null
    : null;

  return (
    <div className="max-w-3xl mx-auto px-6 py-10 sm:py-14 space-y-7">
      <header className="space-y-3">
        <p className="comment">// compare CPU and GPU rental-price scenarios</p>
        <h1 className="text-2xl sm:text-3xl font-bold accent">Compute earnings estimates</h1>
        <p className="text-sm leading-6 text-[var(--color-fg)]">
          Choose a CPU or GPU reference, then compare its cloud rental price at different assumed paid-use levels. These scenarios are not c0mpute payout forecasts.
        </p>
        <p className="text-xs text-[var(--color-dim)]">
          Live network: {online === null ? "data unavailable" : `${online.toLocaleString()} nodes online`} · {jobsCompleted24h === null ? "earnings activity unavailable" : `${jobsCompleted24h.toLocaleString()} jobs completed in the last 24 hours`}.
        </p>
      </header>

      <EarningsEstimator jobsCompleted24h={jobsCompleted24h} cpuWorkersOnline={cpuWorkersOnline} />

      <p className="text-xs text-[var(--color-dim)]">
        Ready to join? <Link href="/providers">Run a node</Link> · <Link href="/market">See current providers</Link> · <Link href="/pricing">How pricing works</Link>
      </p>
    </div>
  );
}
