import Link from "next/link";
import DashboardShell from "@/components/dashboard-shell";
import OverviewGrid from "@/components/overview-grid";
import type { OverviewCard } from "@/components/overview-grid";
import ResourceTable from "@/components/resource-table";
import LiveBadge from "@/components/live-badge";
import { getStatusPayload } from "@/lib/status";
import type { StatusPayload } from "@/lib/status";

export const metadata = { title: "status — c0mpute" };
export const revalidate = 30;

export default async function StatusPage() {
  const status: StatusPayload = await getStatusPayload();
  const { network } = status;

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

  return (
    <DashboardShell>
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
