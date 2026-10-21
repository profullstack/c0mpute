import Link from "next/link";
import { CodeBlock } from "@/components/CodeBlock";
import { getNodesPayload, NODE_GOAL } from "@/lib/nodes";

export const metadata = {
  title: "nodes — c0mpute",
  description:
    "Every c0mpute node online right now, ranked by its c0mpute bench score, plus the people who invited them. The goal is 100,000 nodes.",
  alternates: { canonical: "https://c0mpute.com/nodes" },
};

// A live board must not be a build-time snapshot. The aggregator read is a
// loopback call over in-memory state.
export const dynamic = "force-dynamic";

function ago(s: number): string {
  if (s < 90) return `${s}s ago`;
  return `${Math.round(s / 60)}m ago`;
}

export default async function NodesPage() {
  const p = await getNodesPayload();
  const online = p.workers_online;
  const pct = Math.max(0.5, Math.min(100, (online / NODE_GOAL) * 100));

  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">nodes</h1>
        <p className="comment">// every node online now, best benchmark first</p>
      </header>

      <section className="space-y-2">
        <div className="flex flex-wrap items-baseline justify-between gap-2 text-sm">
          <span>
            <span className="accent font-bold text-lg">{online.toLocaleString()}</span> nodes online
          </span>
          <span className="text-xs text-[var(--color-dim)]">goal: {NODE_GOAL.toLocaleString()}</span>
        </div>
        <div className="h-1.5 w-full rounded bg-[var(--color-rule)] overflow-hidden" aria-hidden="true">
          <div className="h-full bg-[var(--color-accent)]" style={{ width: `${pct}%` }} />
        </div>
        <p className="text-xs text-[var(--color-dim)] leading-6">
          Read from the signed capability adverts each worker gossips every
          minute. A node drops off five minutes after its last advert. Each row
          shows only what the advert already says in public: the end of the
          peer id, a hardware class, cores, the bench score, the version and the
          referral handle it was installed with.
          {p.source === "unavailable" ? " The aggregator is not answering right now, so the board is empty." : ""}
        </p>
      </section>

      <section className="space-y-3">
        <h2 className="text-sm font-semibold">[ board ]</h2>
        {p.nodes.length === 0 ? (
          <p className="text-sm text-[var(--color-dim)]">No nodes seen yet. Yours can be the first:</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-left border-collapse text-xs">
              <thead className="text-[var(--color-dim)]">
                <tr>
                  <th className="py-2 pr-3">#</th>
                  <th className="py-2 pr-3">node</th>
                  <th className="py-2 pr-3">class</th>
                  <th className="py-2 pr-3">cores</th>
                  <th className="py-2 pr-3">bench</th>
                  <th className="py-2 pr-3">version</th>
                  <th className="py-2 pr-3">invited by</th>
                  <th className="py-2">seen</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-[var(--color-rule)]">
                {p.nodes.map((n, i) => (
                  <tr key={`${n.id}-${i}`}>
                    <td className="py-2 pr-3 text-[var(--color-dim)]">{i + 1}</td>
                    <td className="py-2 pr-3 accent font-mono">{n.id}</td>
                    <td className="py-2 pr-3">{n.class}</td>
                    <td className="py-2 pr-3">{n.cores ?? "-"}</td>
                    <td className="py-2 pr-3">{n.bench_score?.toLocaleString() ?? "-"}</td>
                    <td className="py-2 pr-3 text-[var(--color-dim)]">{n.version ?? "-"}</td>
                    <td className="py-2 pr-3">{n.ref ? <Link href={`/r/${n.ref}`}>{n.ref}</Link> : "-"}</td>
                    <td className="py-2 text-[var(--color-dim)]">{ago(n.last_seen_s)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        <CodeBlock>{`$ curl -fsSL https://c0mpute.com/install.sh | sh
$ c0mpute bench        # one full run puts a score next to your node`}</CodeBlock>
      </section>

      <section className="space-y-3" id="refer">
        <h2 className="text-sm font-semibold">[ invite people ]</h2>
        <p className="text-sm leading-7">
          Pick a handle and share <code>https://c0mpute.com/r/&lt;handle&gt;</code>.
          That page gives your visitors an install line with your handle in it,
          and every node they start shows up here with your name next to it.
          Handles are lowercase letters, digits, <code>-</code> and{" "}
          <code>_</code>, up to 32 characters.
        </p>
        <p className="text-xs text-[var(--color-dim)] leading-6">
          It is a public scoreboard. Handles are not verified and carry no
          payout. Already running a node someone told you about? Credit them
          with <code>echo &lt;handle&gt; &gt; ~/.c0mpute/ref</code> and restart
          the worker.
        </p>
        {p.referrers.length > 0 && (
          <table className="w-full text-left border-collapse text-xs">
            <thead className="text-[var(--color-dim)]">
              <tr>
                <th className="py-2 pr-3">handle</th>
                <th className="py-2">nodes online</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-[var(--color-rule)]">
              {p.referrers.map((r) => (
                <tr key={r.ref}>
                  <td className="py-2 pr-3 accent">
                    <Link href={`/r/${r.ref}`}>{r.ref}</Link>
                  </td>
                  <td className="py-2">{r.nodes}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      <p className="text-xs text-[var(--color-dim)]">
        → <Link href="/providers">running a node</Link> · <Link href="/status">network status</Link> ·{" "}
        <a href="/api/nodes">/api/nodes</a> (JSON)
      </p>
    </div>
  );
}
