import Link from "next/link";
import { CodeBlock } from "@/components/CodeBlock";
import { getNodesPayload, NODE_GOAL } from "@/lib/nodes";
import { pageOg } from "@/lib/og";

const DESCRIPTION =
  "Every c0mpute node online right now, ranked by its c0mpute bench score, with who runs each one and who invited them. How to name your node. The goal is 100,000 nodes.";

export const metadata = {
  title: "nodes — c0mpute",
  description: DESCRIPTION,
  alternates: { canonical: "https://c0mpute.com/nodes" },
  ...pageOg("c0mpute nodes online", DESCRIPTION, "/nodes"),
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
          shows only what the advert already says in public: the operator&apos;s
          nickname, the end of the peer id, a hardware class, cores, the bench
          score, the version and the referral handle it was installed with.
          {p.source === "unavailable" ? " The aggregator is not answering right now, so the board is empty." : ""}
        </p>
      </section>

      <p className="text-xs text-[var(--color-dim)]">
        See <Link href="/benchmark">hardware details and per-workload benchmarks</Link>.
      </p>

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
                    <td className="py-2 pr-3">
                      {n.name ? <span className="accent font-bold">{n.name}</span> : <span className="text-[var(--color-dim)]">unnamed</span>}
                      <span className="block font-mono text-[var(--color-dim)]">{n.id}</span>
                      {n.donates?.includes("crowdgpt") && (
                        <Link href="/plugins#crowdgpt" className="block text-[var(--color-dim)] !border-0" title="Donates idle compute to CrowdGPT when it has no paid work">
                          donates idle to CrowdGPT
                        </Link>
                      )}
                      {n.duplicate && (
                        <Link
                          href="/docs#duplicate-workers"
                          className="block text-red-500 !border-0"
                          title="This identity runs as more than one process. Run `c0mpute doctor` on each machine."
                        >
                          duplicate identity{n.duplicate.instances > 1 ? ` (${n.duplicate.instances} processes)` : ""}
                          {n.duplicate.yielding ? ", older one paused" : ""}
                        </Link>
                      )}
                    </td>
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
        <CodeBlock>{`$ curl -fsSL https://c0mpute.com/install.sh | sh -s -- --name <your-handle>
$ c0mpute bench        # one full run puts a score next to your node`}</CodeBlock>
      </section>

      <section className="space-y-3" id="name">
        <h2 className="text-sm font-semibold">[ name your node ]</h2>
        <p className="text-sm leading-7">
          Every row starts as <span className="text-[var(--color-dim)]">unnamed</span>{" "}
          with only the last 8 characters of its peer id. Give it a nickname so
          people can see who runs it. Use the same nickname on every machine
          you run and they are counted together under{" "}
          <a href="#operators">operators</a>.
        </p>
        <p className="text-sm leading-7">On a node that is already running:</p>
        <CodeBlock>{`$ c0mpute name profullstack       # set it (use your own handle)
$ c0mpute worker stop && c0mpute worker start -d   # restart to publish it
$ c0mpute name                    # check what is set`}</CodeBlock>
        <p className="text-sm leading-7">When installing a new one:</p>
        <CodeBlock>{`$ curl -fsSL https://c0mpute.com/install.sh | sh -s -- --name profullstack`}</CodeBlock>
        <p className="text-xs text-[var(--color-dim)] leading-6">
          Your row shows the new name within a minute of the restart, on the
          worker&apos;s next advert. The nickname is stored in{" "}
          <code>~/.c0mpute/name</code>; the <code>C0MPUTE_NAME</code> environment
          variable overrides it (handy in Docker or systemd).{" "}
          <code>c0mpute name --clear</code> removes it. Rules: lowercase letters,
          digits, <code>-</code> and <code>_</code>, up to 32 characters.
          Anything else is ignored and the row stays unnamed. Names are public
          and nobody checks them, so they show who says they run a node, not
          who gets paid: earnings go to the payout address set on the node
          (<code>infernet payout set</code>), never to a name. Needs c0mpute 0.2.64 or newer (<code>c0mpute update</code>).
        </p>
        {p.operators.length > 0 && (
          <table id="operators" className="w-full text-left border-collapse text-xs">
            <thead className="text-[var(--color-dim)]">
              <tr>
                <th className="py-2 pr-3">operator</th>
                <th className="py-2">nodes online</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-[var(--color-rule)]">
              {p.operators.map((o) => (
                <tr key={o.name}>
                  <td className="py-2 pr-3 accent">{o.name}</td>
                  <td className="py-2">{o.nodes}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      <section className="space-y-3" id="refer">
        <h2 className="text-sm font-semibold">[ invite people ]</h2>
        <p className="text-sm leading-7">
          This is separate from your node&apos;s name: a name says who runs a
          node, a referral handle says who brought it in. Pick a handle and
          share <code>https://c0mpute.com/r/&lt;handle&gt;</code>. That page
          gives your visitors an install line with your handle in it, and every
          node they start shows your handle in the <em>invited by</em> column.
          Same rules as names: lowercase letters, digits, <code>-</code> and{" "}
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
