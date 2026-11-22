import Link from "next/link";
import { CodeBlock } from "@/components/CodeBlock";
import {
  duration,
  getNodesPayload,
  NODE_GOAL,
  operatorEarnings,
  summarizeByType,
  usd,
} from "@/lib/nodes";
import { MARKET_FEE_PCT } from "@/lib/market";
import { pageOg } from "@/lib/og";
import { getNetworkEarnings } from "@/lib/earnings/network-server";

const DESCRIPTION =
  "Every c0mpute node online right now: jobs each one ran in the last 24 hours by type, how long they took, what they earned, who runs each node and who invited them. The goal is 100,000 nodes.";

export const metadata = {
  title: "nodes — c0mpute",
  description: DESCRIPTION,
  alternates: { canonical: "https://c0mpute.com/nodes" },
  ...pageOg("c0mpute nodes online", DESCRIPTION, "/nodes"),
};

// A live board must not be a build-time snapshot. The aggregator read is a
// loopback call over in-memory state.
export const dynamic = "force-dynamic";

/** Short label for a workload type: "infernet.inference" -> "inference". */
function jobLabel(t: string): string {
  const known: Record<string, string> = {
    "infernet.inference": "chat / inference",
    "tts.synthesize": "text to speech",
    "stt.transcribe": "speech to text",
    "diffusion.txt2img": "images",
    "music.txt2music": "music",
    "video.txt2vid": "video",
    "video.img2vid": "image to video",
    "ffmpeg.transcode": "transcode",
    "workflow.step": "workflow step",
    "container.run": "container",
    "infernet.temporal.train": "fine-tune",
    "infernet.temporal.infer": "temporal inference",
    unknown: "other",
  };
  return known[t] ?? t;
}

function ago(s: number): string {
  if (s < 90) return `${s}s ago`;
  return `${Math.round(s / 60)}m ago`;
}

export default async function NodesPage() {
  const [p, earnings] = await Promise.all([getNodesPayload(), getNetworkEarnings().catch(() => null)]);
  const inf = earnings?.infernet ?? null;
  const money = (n: number) => (n >= 0.01 || n === 0 ? `$${n.toFixed(2)}` : `$${n.toFixed(4)}`);
  const online = p.workers_online;
  const pct = Math.max(0.5, Math.min(100, (online / NODE_GOAL) * 100));
  const byType = summarizeByType(p.nodes);
  const totalJobs = byType.reduce((a, t) => a + t.jobs, 0);
  const totalEarned = operatorEarnings(byType.reduce((a, t) => a + t.paid_usd, 0));
  const earningNodes = p.nodes.filter((n) => (n.jobs_24h?.jobs ?? 0) > 0).length;

  return (
    <div className="max-w-5xl mx-auto px-4 sm:px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">nodes</h1>
        <p className="comment">// every node online now, best benchmark first, with what it ran and earned in the last 24h</p>
        <p className="text-sm">
          Run a node? <Link href="/dashboard/nodes">See your own earnings vs spend</Link> over the last hour to 30 days, and your <Link href="/dashboard/disk">hosted sites and disk earnings</Link>.
        </p>
      </header>

      {inf && (
        <section className="space-y-3 border border-[var(--color-rule)] bg-[var(--color-card)] rounded-md p-4 sm:p-5" id="earnings">
          <h2 className="text-sm font-semibold">[ what running a node pays ]</h2>
          <div className="grid gap-4 sm:grid-cols-3 text-sm">
            <div>
              <p className="text-xs text-[var(--color-dim)]">paid to node operators</p>
              <p className="accent font-bold text-lg tabular-nums">{money(inf.all.earned_usd)}</p>
              <p className="text-xs text-[var(--color-dim)]">
                settled · {money(inf["30d"].earned_usd)} in 30 days · {inf.all.earning_nodes} nodes paid · best {money(inf.all.best_node_usd)}
              </p>
            </div>
            <div>
              <p className="text-xs text-[var(--color-dim)]">operators keep</p>
              <p className="accent font-bold text-lg tabular-nums">{100 - MARKET_FEE_PCT}%</p>
              <p className="text-xs text-[var(--color-dim)]">
                house commission {money(inf.all.house_fee_usd)}, {inf.all.house_fee_pct}% of {money(inf.all.billed_usd)} billed
              </p>
            </div>
            <div>
              <p className="text-xs text-[var(--color-dim)]">refer a node, earn a commission</p>
              <p className="accent font-bold text-lg tabular-nums">{money(inf.all.commissions_usd)}</p>
              <p className="text-xs text-[var(--color-dim)]">
                paid to {inf.all.earning_referrers} referrers so far · 5% of the house fee on every job a node you referred serves, never out of the operator&apos;s share
              </p>
            </div>
          </div>
          <p className="text-xs text-[var(--color-dim)] leading-6">
            Real ledger numbers, house subsidies left out. <Link href="/status#earnings">Full breakdown</Link> ·{" "}
            <a href="https://infernetprotocol.com/refer">Get your referral link</a> ·{" "}
            <Link href="/estimates">What a GPU needs to pay back</Link> · <Link href="/providers">Run a node</Link>
          </p>
        </section>
      )}

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

      <section className="space-y-3" id="jobs">
        <h2 className="text-sm font-semibold">[ last 24h: what nodes earned, by job type ]</h2>
        {totalJobs === 0 ? (
          <p className="text-sm text-[var(--color-dim)]">
            No completed jobs heard in the last 24 hours yet (the window restarts when the aggregator does).
          </p>
        ) : (
          <>
            <p className="text-sm leading-7">
              <span className="accent font-bold">{earningNodes}</span> nodes ran{" "}
              <span className="accent font-bold">{totalJobs.toLocaleString()}</span> jobs at an agreed{" "}
              <span className="accent font-bold">{usd(totalEarned)}</span> after the {MARKET_FEE_PCT}% fee (settles when market settlement launches; chat is paid through Infernet, above).
            </p>
            <div className="overflow-x-auto">
              <table className="w-full text-left border-collapse text-xs">
                <thead className="text-[var(--color-dim)]">
                  <tr>
                    <th className="py-2 pr-3">job type</th>
                    <th className="py-2 pr-3">jobs</th>
                    <th className="py-2 pr-3">nodes</th>
                    <th className="py-2 pr-3">avg run time</th>
                    <th className="py-2 pr-3">avg price</th>
                    <th className="py-2 pr-3">node keeps / job</th>
                    <th className="py-2">node keeps / busy hour</th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-[var(--color-rule)]">
                  {byType.map((t) => (
                    <tr key={t.type}>
                      <td className="py-2 pr-3">
                        {jobLabel(t.type)}
                        <span className="block font-mono text-[var(--color-dim)]">{t.type}</span>
                      </td>
                      <td className="py-2 pr-3">{t.jobs.toLocaleString()}</td>
                      <td className="py-2 pr-3">{t.nodes}</td>
                      <td className="py-2 pr-3">{duration(t.busy_s / t.jobs)}</td>
                      <td className="py-2 pr-3">{usd(t.paid_usd / t.jobs)}</td>
                      <td className="py-2 pr-3">{usd(operatorEarnings(t.paid_usd) / t.jobs)}</td>
                      <td className="py-2 accent">
                        {t.busy_s > 0 ? usd(operatorEarnings(t.paid_usd) / (t.busy_s / 3600)) : "-"}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </>
        )}
        <p className="text-xs text-[var(--color-dim)] leading-6">
          Counted from the job accepts and receipts every node gossips: a job is
          credited to a node only when its receipt comes from the peer the buyer
          accepted. Run time is accept to receipt. Price is what the buyer agreed
          to before the {MARKET_FEE_PCT}% house fee; the node keeps the rest. Free
          jobs count at $0. Demand is early: these are real numbers, not a promise.
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
                  <th className="py-2 pr-3" title="Completed jobs in the last 24h, by type">jobs 24h</th>
                  <th className="py-2 pr-3" title="Accept-to-receipt time summed over those jobs">busy</th>
                  <th className="py-2 pr-3" title={`What buyers agreed to pay, minus the ${MARKET_FEE_PCT}% fee`}>agreed</th>
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
                      {n.contact && /^[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z0-9-]+$/.test(n.contact) && (
                        <a href={`mailto:${n.contact}`} className="block text-[var(--color-dim)] !border-0" title="The operator's contact for questions about this node">
                          {n.contact}
                        </a>
                      )}
                      {n.dm && (
                        <span className="block font-mono text-[var(--color-dim)]" title="Message whoever runs this node (end-to-end encrypted)">
                          c0mpute dm send {n.id} &quot;…&quot;
                        </span>
                      )}
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
                    {n.jobs_24h && n.jobs_24h.jobs > 0 ? (
                      (() => {
                        const j = n.jobs_24h;
                        const earned = operatorEarnings(j.paid_usd);
                        return (
                          <>
                            <td className="py-2 pr-3">
                              {j.jobs.toLocaleString()}
                              {Object.entries(j.by_type)
                                .sort((a, b) => b[1].jobs - a[1].jobs)
                                .slice(0, 3)
                                .map(([t, w]) => (
                                  <span key={t} className="block text-[var(--color-dim)] whitespace-nowrap">
                                    {jobLabel(t)} {w.jobs}
                                  </span>
                                ))}
                            </td>
                            <td className="py-2 pr-3">{duration(j.busy_s)}</td>
                            <td className="py-2 pr-3 accent">{usd(earned)}</td>
                          </>
                        );
                      })()
                    ) : (
                      <>
                        <td className="py-2 pr-3 text-[var(--color-dim)]">0</td>
                        <td className="py-2 pr-3 text-[var(--color-dim)]">-</td>
                        <td className="py-2 pr-3 text-[var(--color-dim)]">-</td>
                      </>
                    )}
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
        <p className="text-sm leading-7" id="contact">
          Add a contact email too, so people who use your node (and the
          network&apos;s maintainers, if it starts failing jobs) can reach you.
          It is optional and shown publicly under your node&apos;s name:
        </p>
        <CodeBlock>{`$ c0mpute contact ops@example.com   # set it, then restart the worker
$ curl -fsSL https://c0mpute.com/install.sh | sh -s -- --contact ops@example.com`}</CodeBlock>
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
        <Link href="/leaderboard">leaderboard</Link> ·{" "}
        <a href="/api/nodes">/api/nodes</a> (JSON)
      </p>
    </div>
  );
}
