import type { Metadata } from "next";
import Link from "next/link";
import { EarningsEstimator, type ObservedEarnings } from "@/components/earnings-estimator";
import { getStatusPayload } from "@/lib/status";
import { getGpuModelPrice, getNetworkEarnings } from "@/lib/earnings/network-server";
import type { NetworkEarnings } from "@/lib/earnings/network";
import { networkObserved } from "@/lib/earnings/server";
import { liveRates } from "@/lib/r4ck-rates-server";

export const metadata: Metadata = {
  title: "Compute earnings estimates — c0mpute",
  description: "What c0mpute node operators actually earned, what a rented GPU has to sell to pay back 1x or 5x, and CPU and GPU rental-price scenarios.",
  alternates: { canonical: "https://c0mpute.com/estimates" },
};

export const dynamic = "force-dynamic";

/**
 * Real earnings for the estimator: settled money only (Infernet credits). c0mpute
 * auction prices are agreed, not paid, until market settlement launches.
 */
function observedFrom(e: NetworkEarnings | null): ObservedEarnings | null {
  if (!e?.infernet) return null;
  const inf = e.infernet;
  const inf7 = inf["7d"];
  return {
    earned30d: inf["30d"].earned_usd,
    earned7d: inf7.earned_usd,
    earnedAll: inf.all.earned_usd,
    earningNodes30d: inf["30d"].earning_nodes,
    medianNode30d: inf["30d"].median_node_usd,
    bestNode30d: inf["30d"].best_node_usd,
    paidJobsPerDay: (inf7?.jobs ?? 0) / 7,
    avgPaidJobUsd: inf7 && inf7.jobs > 0 ? inf7.billed_usd / inf7.jobs : 0,
  };
}

export default async function EstimatesPage() {
  const [status, earnings, gpuPrice] = await Promise.all([
    getStatusPayload(),
    getNetworkEarnings().catch(() => null),
    getGpuModelPrice().catch(() => null),
  ]);
  const [rates, byClass] = await Promise.all([liveRates(), networkObserved()]);
  const jobsCompleted24h = status.source === "aggregator" ? status.network.jobs_completed_24h : null;
  const online = status.source === "aggregator" ? status.network.workers_online : null;
  const cpuWorkersOnline = status.source === "aggregator"
    ? status.network.workers_with_tag["c0mpute:cpu"] ?? null
    : null;
  const observed = observedFrom(earnings);
  const usd = (n: number) => `$${n.toFixed(2)}`;

  return (
    <div className="max-w-3xl mx-auto px-6 py-10 sm:py-14 space-y-7">
      <header className="space-y-3">
        <p className="comment">// what nodes earn, and what a GPU has to sell to pay for itself</p>
        <h1 className="text-2xl sm:text-3xl font-bold accent">Compute earnings estimates</h1>
        {observed && (
          <p className="text-sm leading-6 text-[var(--color-fg)]">
            Node operators were paid <strong className="accent">{usd(observed.earned30d)}</strong> in the last 30 days
            ({usd(observed.earnedAll)} all time) across {observed.earningNodes30d.toLocaleString()} nodes.{" "}
            <Link href="/status#earnings">Real ledger numbers</Link>.
          </p>
        )}
        <p className="text-sm leading-6 text-[var(--color-fg)]">
          Choose a CPU or GPU reference to compare its current rental price (live from r4ck.dev) at different paid-use levels, and see how much work a
          rented GPU has to sell to pay its own rent, or to pay for four more.
        </p>
        <p className="text-xs text-[var(--color-dim)]">
          Live network: {online === null ? "data unavailable" : `${online.toLocaleString()} nodes online`} · {jobsCompleted24h === null ? "activity unavailable" : `${jobsCompleted24h.toLocaleString()} jobs completed in the last 24 hours`}.
        </p>
      </header>

      <EarningsEstimator
        jobsCompleted24h={jobsCompleted24h}
        cpuWorkersOnline={cpuWorkersOnline}
        observed={observed}
        gpuPrice={gpuPrice}
        liveRates={rates}
        observedByClass={byClass ? { history_from: byClass.history_from, windows: byClass.windows } : null}
      />

      <p className="text-xs text-[var(--color-dim)]">
        Ready to join? <Link href="/providers">Run a node</Link> · <Link href="/status#earnings">What nodes earned</Link> · <Link href="/market">See current providers</Link> · <Link href="/pricing">How pricing works</Link>
      </p>
    </div>
  );
}
