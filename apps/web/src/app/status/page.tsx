import Link from "next/link";
import DashboardShell from "@/components/dashboard-shell";
import OverviewGrid from "@/components/overview-grid";
import type { OverviewCard } from "@/components/overview-grid";
import ResourceTable from "@/components/resource-table";
import LiveBadge from "@/components/live-badge";
import { getStatusPayload } from "@/lib/status";
import type { StatusPayload } from "@/lib/status";
import { getNetworkEarnings } from "@/lib/earnings/network-server";
import type { NetworkEarnings, WindowKey } from "@/lib/earnings/network";
import { MARKET_FEE_PCT } from "@/lib/market";

export const metadata = { title: "status — c0mpute" };
// Always render fresh against the aggregator. A live status surface must not
// serve a stale build-time snapshot — with ISR (`revalidate`) the page could
// be frozen as the "aggregator not deployed" stub even after the aggregator is
// up. The aggregator read is a cheap loopback call that serves in-memory data.
export const dynamic = "force-dynamic";

export default async function StatusPage() {
  const [status, earnings]: [StatusPayload, NetworkEarnings | null] = await Promise.all([
    getStatusPayload(),
    getNetworkEarnings().catch(() => null),
  ]);
  const { network } = status;

  // ── earnings: c0mpute jobs + Infernet inference, never double counted ──
  const usd = (n: number) => (n >= 100 ? `$${n.toFixed(0)}` : n >= 0.01 || n === 0 ? `$${n.toFixed(2)}` : `$${n.toFixed(4)}`);
  const windowLabel: Record<WindowKey, string> = { "24h": "last 24h", "7d": "last 7 days", "30d": "last 30 days", all: "all time" };
  const earned = (k: WindowKey) => (earnings?.c0mpute?.[k]?.earned_usd ?? 0) + (earnings?.infernet?.[k]?.earned_usd ?? 0);
  const allInf = earnings?.infernet?.all;
  const allC0 = earnings?.c0mpute?.all;
  const earningCards: OverviewCard[] = earnings && (earnings.c0mpute || earnings.infernet)
    ? [
        {
          label: "Paid to node operators",
          value: usd(earned("all")),
          note: `all time · c0mpute jobs ${usd(allC0?.earned_usd ?? 0)} · Infernet inference ${usd(allInf?.earned_usd ?? 0)}`,
        },
        {
          label: "Last 30 days",
          value: usd(earned("30d")),
          note: `${usd(earned("7d"))} in 7 days · ${usd(earned("24h"))} in 24h`,
        },
        {
          label: "House commission",
          value: usd(allInf?.house_fee_usd ?? 0),
          note: allInf && allInf.billed_usd > 0
            ? `${allInf.house_fee_pct}% of ${usd(allInf.billed_usd)} billed inference · operators keep the rest`
            : "no billed inference yet",
        },
        {
          label: "Referral commissions",
          value: usd(allInf?.commissions_usd ?? 0),
          note: `${allInf?.earning_referrers ?? 0} referrers earning · paid from the house fee, never the operator's share`,
        },
        {
          label: "Operators who earned",
          value: Math.max(allC0?.earning_nodes ?? 0, allInf?.earning_nodes ?? 0),
          note: `best node ${usd(Math.max(allC0?.best_node_usd ?? 0, allInf?.best_node_usd ?? 0))} · paid out ${usd(allInf?.paid_out_usd ?? 0)} to wallets`,
        },
      ]
    : [];
  const earningRows = (["24h", "7d", "30d", "all"] as WindowKey[]).map((k) => ({
    id: k,
    window: windowLabel[k],
    c0mpute: earnings?.c0mpute ? `${usd(earnings.c0mpute[k].earned_usd)} · ${earnings.c0mpute[k].paid_jobs.toLocaleString()} paid jobs` : "—",
    infernet: earnings?.infernet ? `${usd(earnings.infernet[k].earned_usd)} · ${earnings.infernet[k].jobs.toLocaleString()} jobs` : "—",
    total: usd(earned(k)),
    commission: earnings?.infernet ? usd(earnings.infernet[k].house_fee_usd) : "—",
    referral: earnings?.infernet ? usd(earnings.infernet[k].commissions_usd) : "—",
    median: usd(Math.max(earnings?.c0mpute?.[k]?.median_node_usd ?? 0, earnings?.infernet?.[k]?.median_node_usd ?? 0)),
  }));

  const overviewCards: OverviewCard[] = [
    {
      label: "Workers online",
      value: network.workers_online,
      note: `${Object.values(network.workers_with_role).reduce((a: number, b: number) => a + b, 0)} roles assigned`,
    },
    {
      label: "Jobs in flight",
      value: network.jobs_in_flight,
      note: "active now",
    },
    {
      label: "Jobs completed (24h)",
      value: network.jobs_completed_24h.toLocaleString(),
      note: "rolling 24h",
    },
    {
      label: "Avg job latency",
      value:
        network.avg_job_latency_seconds === null
          ? "\u2014"
          : `${network.avg_job_latency_seconds.toFixed(1)}s`,
      note: "per workload",
    },
    {
      label: "Bench capacity",
      value:
        network.bench && network.bench.workers_benchmarked > 0
          ? network.bench.total_score.toLocaleString()
          : "\u2014",
      note: network.bench
        ? `${network.bench.workers_benchmarked} benchmarked \u00b7 median ${network.bench.median_score ?? "\u2014"}`
        : "run `c0mpute bench` on a worker",
    },
  ];

  const roleRows = Object.entries(network.workers_with_role)
    .sort(([, a], [, b]) => b - a)
    .map(([role, count]) => {
      const total = Object.values(network.workers_with_role).reduce(
        (a, b) => a + b,
        0,
      );
      const share =
        total > 0 ? `${((count / total) * 100).toFixed(0)}%` : "\u2014";
      return { id: role, role, count, share };
    });

  const tagRows = Object.entries(network.workers_with_tag)
    .sort(([, a], [, b]) => b - a)
    .map(([tag, count]) => {
      const label = tag.replace(/^c0mpute:/, "");
      return { id: tag, tag: label, count };
    });

  const workloadRows = Object.entries(network.workload_types)
    .sort(([, a], [, b]) => b.jobs_in_flight - a.jobs_in_flight)
    .map(([type, stats]) => ({
      id: type,
      type,
      in_flight: stats.jobs_in_flight,
      completed_24h: stats.jobs_completed_24h.toLocaleString(),
      latency:
        stats.avg_latency_seconds === null
          ? "\u2014"
          : `${stats.avg_latency_seconds.toFixed(1)}s`,
    }));

  const pct = (r: number | null | undefined) =>
    r === null || r === undefined ? "\u2014" : `${Math.round(r * 100)}%`;
  const canaryRows = Object.entries(network.auction_canary ?? {})
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([type, c]) => ({
      id: type,
      type,
      probes: c.auctions,
      bid_rate: pct(c.success_rate),
      capable_rate:
        c.with_capable && c.with_capable > 0
          ? `${pct(c.capable_success_rate)} of ${c.with_capable}`
          : "no capable node",
      last: c.last_bids === null ? "\u2014" : `${c.last_bids} bids`,
    }));

  return (
    <DashboardShell>
      {/* An aggregator is an optional hosted service, and saying so is not a
          disclaimer — it is the architecture. A buyer who doubts this view
          queries the network directly. See DIP-0024. */}
      <p className="text-xs text-[var(--color-dim)] leading-6">
        An observed view of the network, not canonical state. This page reads
        from an aggregator we run; aggregators and indexers cache what peers
        publish and can be stale, partial or incomplete. They cannot forge a
        record — every advert and receipt is independently signed — and
        nothing here is authoritative.{" "}
        <Link href="/protocol" className="hover:text-[var(--color-accent)]">
          how that works
        </Link>
      </p>

      <div className="flex items-center justify-between">
        {status.source === "stub" && (
          <span className="text-xs text-[var(--color-warn)]">
            aggregator not deployed &mdash; showing placeholder data
          </span>
        )}
        {status.source === "aggregator" && <span />}
        <LiveBadge />
      </div>

      <OverviewGrid cards={overviewCards} />

      <section id="earnings" className="space-y-4">
        <div className="flex flex-wrap items-baseline justify-between gap-2">
          <h2 className="text-sm font-semibold">[ what node operators earned ]</h2>
          <Link href="/providers" className="text-xs hover:text-[var(--color-accent)]">
            run a node and earn &rarr;
          </Link>
        </div>
        {earningCards.length === 0 ? (
          <p className="text-sm text-[var(--color-dim)]">Earnings ledgers are not answering right now.</p>
        ) : (
          <>
            <OverviewGrid cards={earningCards} />
            <ResourceTable
              title="[ earnings by window ]"
              description="Money nodes kept, by where the work came from. Median is the typical earning node in that window."
              columns={[
                { key: "window", label: "Window" },
                { key: "c0mpute", label: "c0mpute jobs" },
                { key: "infernet", label: "Infernet inference" },
                { key: "total", label: "Total to operators" },
                { key: "commission", label: "House commission" },
                { key: "referral", label: "Referral commissions" },
                { key: "median", label: "Median node" },
              ]}
              rows={earningRows}
              emptyMessage="No earnings yet."
            />
          </>
        )}
        <p className="text-xs text-[var(--color-dim)] leading-6">
          Real money, not estimates. c0mpute jobs come from the signed accept and
          receipt each node gossips, copied into our ledger so history outlives the
          aggregator&apos;s 24h window
          {earnings?.history_from ? ` (on file since ${new Date(earnings.history_from).toISOString().slice(0, 10)})` : ""};
          shown after the {MARKET_FEE_PCT}% fee, as on <Link href="/nodes#jobs">/nodes</Link>. Infernet
          inference is the operator&apos;s net from the{" "}
          <a href="https://infernetprotocol.com/api/v1/network/earnings">Infernet earnings ledger</a>, with
          house subsidies and voided jobs left out. An inference job a c0mpute node serves is
          paid through Infernet only, so nothing is counted twice. Referral commissions come
          out of the house commission. No node, account or wallet is named. Demand is early:
          these are real numbers, not a promise.
        </p>
      </section>

      <div className="grid gap-6 xl:grid-cols-2">
        <ResourceTable
          title="[ workers by role ]"
          description="Workers online, grouped by their configured role."
          columns={[
            { key: "role", label: "Role" },
            { key: "count", label: "Workers" },
            { key: "share", label: "Share" },
          ]}
          rows={roleRows}
          emptyMessage="No workers online."
        />

        <ResourceTable
          title="[ workload types ]"
          description="Job throughput by workload type."
          columns={[
            { key: "type", label: "Workload" },
            { key: "in_flight", label: "In flight" },
            { key: "completed_24h", label: "Completed (24h)" },
            { key: "latency", label: "Avg latency" },
          ]}
          rows={workloadRows}
          emptyMessage="No workload activity."
        />
      </div>

      <ResourceTable
        title="[ auction canary ]"
        description="One tiny offer per workload every 10 minutes, bids counted, nothing accepted (last hour). 'With a capable node' counts only probes sent while a node advertised the workload: that is the rate that should stay near 100%."
        columns={[
          { key: "type", label: "Workload" },
          { key: "probes", label: "Probes" },
          { key: "bid_rate", label: "Got a bid" },
          { key: "capable_rate", label: "With a capable node" },
          { key: "last", label: "Last probe" },
        ]}
        rows={canaryRows}
        emptyMessage="No canary probes yet (the aggregator probes every 10 minutes)."
      />

      <ResourceTable
        title="[ capability tags ]"
        description="Workers advertising each capability tag. Tags are colon-separated and hierarchical (c0mpute:role:storage, c0mpute:gpu:nvidia, etc.)."
        columns={[
          { key: "tag", label: "Capability" },
          { key: "count", label: "Workers" },
        ]}
        rows={tagRows}
        emptyMessage="No capability advertisements received."
      />

      <footer className="text-xs text-[var(--color-dim)] space-y-2 pt-6 rule">
        <p>
          All values are aggregates across the full p2p network. No individual
          worker, job, customer, DID, IP address, or job input/output data is
          displayed. See{" "}
          <a href="https://github.com/profullstack/c0mpute/blob/master/dips/0014-status-aggregator.md">
            DIP-0014
          </a>{" "}
          for the privacy model and aggregator design.
        </p>
        {status.source === "aggregator" && (
          <p>
            generated at {status.generated_at} &middot; source:{" "}
            <span className="text-[var(--color-accent)]">aggregator</span>
          </p>
        )}
        <p>
          &rarr;{" "}
          <Link href="/getting-started">getting-started</Link> &middot;{" "}
          <Link href="/docs">docs</Link> &middot;{" "}
          <Link href="/plugins">plugins</Link>
        </p>
      </footer>
    </DashboardShell>
  );
}
