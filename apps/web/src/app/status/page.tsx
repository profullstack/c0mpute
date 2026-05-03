import Link from "next/link";

export const metadata = { title: "status — c0mpute" };
export const revalidate = 30; // serve cached version up to 30s

interface StatusPayload {
  ok: boolean;
  generated_at: string;
  network: {
    workers_online: number;
    workers_with_role: Record<string, number>;
    jobs_in_flight: number;
    jobs_completed_24h: number;
    avg_job_latency_seconds: number | null;
  };
  source: "aggregator" | "stub";
}

async function fetchStatus(): Promise<StatusPayload | null> {
  // Same-origin fetch to /api/status. revalidate caches it.
  try {
    const r = await fetch(
      `${process.env.NEXT_PUBLIC_BASE_URL ?? ""}/api/status`,
      { next: { revalidate: 30 } },
    );
    if (!r.ok) return null;
    return (await r.json()) as StatusPayload;
  } catch {
    return null;
  }
}

export default async function StatusPage() {
  const status = await fetchStatus();

  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">status</h1>
        <p className="comment">// public network health · aggregates only · no private data</p>
      </header>

      {!status ? (
        <p className="text-sm text-[var(--color-dim)]">
          status temporarily unavailable
        </p>
      ) : (
        <>
          <Section title="[ network ]">
            <Row label="workers online" value={status.network.workers_online} />
            <Row
              label="jobs in flight"
              value={status.network.jobs_in_flight}
            />
            <Row
              label="jobs completed (24h)"
              value={status.network.jobs_completed_24h}
            />
            <Row
              label="avg job latency"
              value={
                status.network.avg_job_latency_seconds === null
                  ? "—"
                  : `${status.network.avg_job_latency_seconds.toFixed(1)}s`
              }
            />
          </Section>

          <Section title="[ workers by role ]">
            {Object.entries(status.network.workers_with_role).map(
              ([role, count]) => (
                <Row key={role} label={role} value={count} />
              ),
            )}
          </Section>

          <Section title="[ data source ]">
            <p className="text-sm">
              {status.source === "aggregator"
                ? "live · pulled from a c0mpute verifier aggregator"
                : "placeholder · the aggregator service hasn't been deployed yet"}
            </p>
            <p className="text-xs text-[var(--color-dim)]">
              generated at {status.generated_at}
            </p>
          </Section>

          <p className="text-xs text-[var(--color-dim)] rule pt-6">
            All values are aggregates across the full network. We do not
            display individual workers, jobs, customers, DIDs, IP
            addresses, or job inputs/outputs. See{" "}
            <a href="https://github.com/profullstack/c0mpute/blob/master/dips/0014-status-aggregator.md">
              DIP-0014
            </a>{" "}
            for the privacy model and aggregator design.
          </p>
        </>
      )}

      <p className="text-xs text-[var(--color-dim)] rule pt-6">
        →{" "}
        <Link href="/getting-started">getting-started</Link> ·{" "}
        <Link href="/docs">docs</Link>
      </p>
    </div>
  );
}

function Section({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <section className="space-y-2">
      <h2 className="text-base accent">{title}</h2>
      <div className="pl-5 space-y-1 text-sm">{children}</div>
    </section>
  );
}

function Row({ label, value }: { label: string; value: string | number }) {
  return (
    <div className="flex justify-between font-mono">
      <span className="text-[var(--color-dim)]">{label}</span>
      <span className="text-[var(--color-fg)]">{value}</span>
    </div>
  );
}
