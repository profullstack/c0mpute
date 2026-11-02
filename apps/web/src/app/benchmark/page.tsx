import Link from "next/link";
import { getNodesPayload } from "@/lib/nodes";
import type { BenchSample, NodeRow, WorkloadBenchmark } from "@/lib/nodes";

export const metadata = {
  title: "benchmarks — c0mpute",
  description:
    "Compare c0mpute node hardware and CPU benchmark performance across fibonacci, matrix multiply and BLAKE3 workloads.",
  alternates: { canonical: "https://c0mpute.com/benchmark" },
};

export const dynamic = "force-dynamic";

const WORKLOADS = [
  { id: "fib", name: "fibonacci", description: "recursive CPU work and call overhead", unit: "visits/s" },
  { id: "matmul", name: "matrix multiply", description: "floating point throughput and cache", unit: "FLOP/s" },
  { id: "hash", name: "BLAKE3 hash", description: "hash throughput and memory bandwidth", unit: "bytes/s" },
] as const;

function bestSample(workload: WorkloadBenchmark): BenchSample | undefined {
  return workload.samples.reduce<BenchSample | undefined>(
    (best, sample) => (!best || sample.throughput > best.throughput ? sample : best),
    undefined,
  );
}

function formatRate(id: string, rate: number): string {
  if (id === "fib") {
    if (rate >= 1e9) return `${(rate / 1e9).toFixed(2)} G visits/s`;
    if (rate >= 1e6) return `${(rate / 1e6).toFixed(1)} M visits/s`;
    return `${rate.toFixed(0)} visits/s`;
  }
  if (id === "matmul") return `${(rate / 1e9).toFixed(2)} GFLOP/s`;
  return `${(rate / (1024 ** 3)).toFixed(2)} GiB/s`;
}

function formatRam(bytes: number | null): string {
  return bytes == null ? "—" : `${(bytes / (1024 ** 3)).toFixed(1)} GiB`;
}

function formatDuration(microseconds: number): string {
  if (microseconds < 1000) return `${microseconds} μs`;
  if (microseconds < 1_000_000) return `${(microseconds / 1000).toFixed(1)} ms`;
  return `${(microseconds / 1_000_000).toFixed(2)} s`;
}

function formatDate(value: string | null): string {
  if (!value) return "—";
  const date = new Date(value);
  return Number.isNaN(date.getTime())
    ? value
    : new Intl.DateTimeFormat("en", {
        dateStyle: "medium",
        timeStyle: "short",
        timeZone: "UTC",
      }).format(date) + " UTC";
}

function NodeLabel({ node }: { node: NodeRow }) {
  return (
    <span className="font-mono">
      <span className="accent">{node.id}</span>
      <span className="block text-[var(--color-dim)]">{node.class}</span>
    </span>
  );
}

function PerformanceTable({
  workload,
  nodes,
}: {
  workload: (typeof WORKLOADS)[number];
  nodes: NodeRow[];
}) {
  const rows = nodes.flatMap((node) => {
    const result = node.benchmarks?.[workload.id];
    const peak = result ? bestSample(result) : undefined;
    const single = result?.samples.find((sample) => sample.threads === 1);
    return peak ? [{ node, result: result!, peak, single }] : [];
  });

  return (
    <section className="space-y-3" id={workload.id}>
      <header className="space-y-1">
        <h2 className="text-sm font-semibold">[ {workload.name} ]</h2>
        <p className="text-xs text-[var(--color-dim)]">
          {workload.description} · higher is better · {workload.unit}
        </p>
      </header>
      {rows.length === 0 ? (
        <p className="text-sm text-[var(--color-dim)]">No detailed {workload.name} results published yet.</p>
      ) : (
        <div className="overflow-x-auto border border-[var(--color-rule)] rounded">
          <table className="w-full min-w-[800px] text-left border-collapse text-xs">
            <thead className="text-[var(--color-dim)] bg-[var(--color-card)]">
              <tr>
                <th className="p-3">node / CPU</th>
                  <th className="p-3">input</th>
                <th className="p-3">1 thread</th>
                <th className="p-3">best throughput</th>
                <th className="p-3">threads</th>
                <th className="p-3">speedup</th>
                  <th className="p-3">run time</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-[var(--color-rule)]">
              {rows.map(({ node, result, peak, single }) => (
                <tr key={node.id}>
                  <td className="p-3"><NodeLabel node={node} /></td>
                  <td className="p-3">{result.params || "—"}</td>
                  <td className="p-3">{single ? formatRate(workload.id, single.throughput) : "—"}</td>
                  <td className="p-3 accent">{formatRate(workload.id, peak.throughput)}</td>
                  <td className="p-3">{peak.threads}</td>
                  <td className="p-3">{peak.speedup.toFixed(2)}×</td>
                  <td className="p-3">{formatDuration(peak.duration_us)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

export default async function BenchmarkPage() {
  const payload = await getNodesPayload();
  const nodes = payload.nodes;
  const benchmarked = nodes.filter((node) => node.bench_score != null);
  const detailed = nodes.filter((node) => node.benchmarks && Object.keys(node.benchmarks).length > 0);

  return (
    <main className="max-w-5xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <div className="flex flex-wrap items-baseline justify-between gap-3">
          <h1 className="text-2xl font-bold accent">benchmarks</h1>
          <Link href="/nodes" className="text-xs">← live node board</Link>
        </div>
        <p className="comment">// hardware and measured CPU throughput from online nodes</p>
      </header>

      <section className="grid gap-4 sm:grid-cols-3">
        <div className="border border-[var(--color-rule)] rounded p-4">
          <div className="text-xs text-[var(--color-dim)]">online nodes</div>
          <div className="text-xl accent mt-1">{payload.workers_online.toLocaleString()}</div>
        </div>
        <div className="border border-[var(--color-rule)] rounded p-4">
          <div className="text-xs text-[var(--color-dim)]">with score</div>
          <div className="text-xl accent mt-1">{benchmarked.length.toLocaleString()}</div>
        </div>
        <div className="border border-[var(--color-rule)] rounded p-4">
          <div className="text-xs text-[var(--color-dim)]">with detailed results</div>
          <div className="text-xl accent mt-1">{detailed.length.toLocaleString()}</div>
        </div>
      </section>

      <section className="space-y-3">
        <h2 className="text-sm font-semibold">[ hardware ]</h2>
        {nodes.length === 0 ? (
          <p className="text-sm text-[var(--color-dim)]">
            {payload.source === "unavailable" ? "The node aggregator is not answering." : "No nodes are online."}
          </p>
        ) : (
          <div className="overflow-x-auto border border-[var(--color-rule)] rounded">
            <table className="w-full min-w-[800px] text-left border-collapse text-xs">
              <thead className="text-[var(--color-dim)] bg-[var(--color-card)]">
                <tr>
                  <th className="p-3">node / CPU model</th>
                  <th className="p-3">class</th>
                  <th className="p-3">cores</th>
                  <th className="p-3">RAM</th>
                  <th className="p-3">score</th>
                  <th className="p-3">bench run</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-[var(--color-rule)]">
                {nodes.map((node) => (
                  <tr key={node.id}>
                    <td className="p-3"><NodeLabel node={node} /></td>
                    <td className="p-3">{node.class}</td>
                    <td className="p-3">{node.cores ?? "—"}</td>
                    <td className="p-3">{formatRam(node.ram_bytes)}</td>
                    <td className="p-3 accent">{node.bench_score?.toLocaleString() ?? "—"}</td>
                    <td className="p-3 text-[var(--color-dim)]">{formatDate(node.bench_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        {nodes.some((node) => !node.ram_bytes || !node.benchmarks) && (
          <p className="text-xs text-[var(--color-dim)] leading-6">
            Some online nodes have older or score-only adverts. Their detailed results and RAM appear after the node updates and publishes a full <code>c0mpute bench</code> report.
          </p>
        )}
      </section>

      <section className="space-y-3 border border-[var(--color-rule)] rounded p-4 text-sm leading-7">
        <h2 className="text-sm font-semibold">[ reading the score ]</h2>
        <p>
          The score combines each node&apos;s best fib, matrix multiply and BLAKE3 throughput as a geometric mean against fixed reference rates. <strong>1000</strong> means one reference core&apos;s throughput on all three workloads. It is a relative CPU capacity score, not a percentage or an AI model tokens/sec measurement.
        </p>
        <p className="text-[var(--color-dim)]">
          Fib counts recursive calls per second; matrix multiply reports floating point operations per second; BLAKE3 reports bytes hashed per second. The thread count and speedup show how each workload scales on that machine. c0mpute does not currently publish per-AI-model inference benchmarks here.
        </p>
      </section>

      {WORKLOADS.map((workload) => (
        <PerformanceTable key={workload.id} workload={workload} nodes={nodes} />
      ))}

      <footer className="text-xs text-[var(--color-dim)]">
        Live data from the public node adverts · <Link href="/api/nodes">JSON API</Link> · refresh with <code>c0mpute bench</code> on a node.
      </footer>
    </main>
  );
}
