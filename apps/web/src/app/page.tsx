import type { Metadata } from "next";
import Link from "next/link";
import { CodeBlock } from "@/components/CodeBlock";
import { getStatusPayload } from "@/lib/status";
import { NODE_GOAL } from "@/lib/nodes";

// ad.network publisher claim for c0mpute.com: the meta tag lets ad.network
// verify ownership from the server-rendered HTML. No ad unit on the home page.
const AD_NETWORK_CODE = "vpG989NPYf0fcWqU";

export const metadata: Metadata = {
  other: { "ad.network": AD_NETWORK_CODE },
};

// Live node count above the fold: refresh every minute instead of hourly.
export const revalidate = 60;

export default async function HomePage() {
  const status = await getStatusPayload({ revalidate: 30 });
  const online = status.source === "aggregator" ? status.network.workers_online : null;
  const tags = status.network.workers_with_tag;
  const gpus =
    (tags["c0mpute:gpu:nvidia"] ?? 0) + (tags["c0mpute:gpu:amd"] ?? 0) + (tags["c0mpute:gpu:apple"] ?? 0);
  const pct = online === null ? 0 : Math.max(0.5, Math.min(100, (online / NODE_GOAL) * 100));

  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-12">
      {/* Above the fold, for the person we most need: someone with a machine.
          The pitch is the mission, the CTA is one command, and the proof is the
          live count, however small it is today. */}
      <section className="space-y-5">
        <p className="comment">// the open compute network</p>
        <h1 className="text-3xl font-bold accent">Run a node. Help build open AI.</h1>
        <p className="text-[var(--color-fg)] leading-relaxed">
          c0mpute pools spare CPUs and GPUs from ordinary machines into one open
          network. One command turns your box into a node that serves inference,
          transcoding and storage jobs.
        </p>
        <p className="text-sm text-[var(--color-dim)] leading-relaxed">
          The goal is 100,000 nodes. At that size the network has enough
          capacity to train and serve open-weight models in the open, on
          hardware owned by the people running it, instead of leaving frontier
          AI to a handful of companies. That is the mission, not a claim about
          today.
        </p>

        <div className="border border-[var(--color-rule)] bg-[var(--color-card)] rounded-md px-4 py-3 space-y-2">
          <div className="flex flex-wrap items-baseline justify-between gap-2 text-sm">
            <span>
              <span className="accent font-bold text-lg">
                {online === null ? "?" : online.toLocaleString()}
              </span>{" "}
              nodes online now
              {online !== null && gpus > 0 ? ` (${gpus} with a GPU)` : ""}
            </span>
            <span className="text-xs text-[var(--color-dim)]">goal: {NODE_GOAL.toLocaleString()}</span>
          </div>
          <div className="h-1.5 w-full rounded bg-[var(--color-rule)] overflow-hidden" aria-hidden="true">
            <div className="h-full bg-[var(--color-accent)]" style={{ width: `${pct}%` }} />
          </div>
          <p className="text-xs text-[var(--color-dim)]">
            Counted live from signed node adverts. → <Link href="/nodes">see every node</Link>
          </p>
        </div>

        <div className="space-y-2" id="run">
          <p className="comment">// run a node: Linux, macOS, or Windows via WSL2 (x86_64 / arm64), CPU or GPU</p>
          <CodeBlock>{`$ curl -fsSL https://c0mpute.com/install.sh | sh`}</CodeBlock>
          <p className="text-xs text-[var(--color-dim)] leading-6">
            The installer starts the worker for you (a systemd service where it
            can, otherwise a background daemon). Check it with{" "}
            <code>c0mpute worker status</code>. No account and no port
            forwarding needed to join. → <Link href="/providers">what running a node involves</Link> ·{" "}
            <Link href="/nodes#refer">invite people with your own link</Link>
          </p>
        </div>

        <p className="text-xs text-[var(--color-dim)]">
          Here to buy compute instead? → <Link href="/getting-started">submit a job</Link> ·{" "}
          <Link href="/status">explore the network</Link> ·{" "}
          <Link href="/protocol">read the protocol</Link>
        </p>
      </section>

      {/* The one idea the rest of the site hangs off. Above the fold on
          purpose: a visitor who reads only this should understand that the
          unit of purchase is finished work, not a rented machine. */}
      <section className="space-y-4 rule pt-8">
        <p className="comment">// buy execution, not machines</p>
        <div className="grid gap-3 sm:grid-cols-3">
          <Step
            n="1"
            title="Describe the job"
            body="Workload, deadline, trust level, validation, max price. You never name a machine."
          />
          <Step
            n="2"
            title="The network finds providers"
            body="Capability discovery, then competitive signed offers from providers that qualify."
          />
          <Step
            n="3"
            title="Pay for accepted work"
            body="Validation, then settlement, then a signed receipt you can verify yourself."
          />
        </div>
        <p className="text-xs text-[var(--color-dim)] leading-6">
          You do not pick a server, a region, or a vendor unless you want to.
          You state constraints; your own client — or a gateway you chose —
          picks an offer that satisfies them.
        </p>
      </section>

      <section className="space-y-3">
        <p className="comment">// run a worker (login ties it to your accounts; register mints your DID)</p>
        <CodeBlock>{`$ c0mpute login
$ c0mpute worker register
$ c0mpute worker start --gpu`}</CodeBlock>
      </section>

      <section className="space-y-3">
        <p className="comment">// submit a job</p>
        <CodeBlock>{`$ c0mpute transcode submit input.mov --preset hls
$ c0mpute infernet chat --model qwen2.5:7b "hello"`}</CodeBlock>
      </section>

      <section className="space-y-3">
        <p className="comment">// interactive dashboard</p>
        <CodeBlock>{`$ c0mpute tui`}</CodeBlock>
      </section>

      <section className="space-y-3">
        <p className="comment">// upgrade or remove</p>
        <CodeBlock>{`$ c0mpute update              # check for + apply new release
$ c0mpute uninstall --all     # remove c0mpute and peer binaries`}</CodeBlock>
      </section>

      <section className="space-y-2 rule pt-8">
        <p className="comment">// no one in the middle</p>
        <div className="border border-[var(--color-rule)] rounded-md px-4 py-3 text-sm leading-6 space-y-2">
          <p>
            Every record on the network — provider adverts, job manifests,
            offers, receipts — carries its own signature, so it stays
            verifiable after it leaves the wire. Your identity is a keypair
            you generate locally, with no account anywhere.
          </p>
          <p className="text-[var(--color-dim)]">
            Indexers and gateways can cache, proxy and bill. None of them is
            authoritative.{" "}
            <span className="text-[var(--color-fg)]">
              If c0mpute.com disappears, c0mpute keeps computing.
            </span>
          </p>
          <p className="text-[var(--color-dim)]">
            → <Link href="/protocol">the protocol</Link>
          </p>
        </div>
      </section>

      <section className="space-y-2 rule pt-8">
        <p className="comment">// for ai agents · webmcp</p>
        <div className="border border-[var(--color-rule)] rounded-md px-4 py-3 text-sm leading-6">
          <p>
            This site speaks{" "}
            <a href="https://webmachinelearning.github.io/webmcp/">WebMCP</a>. A
            browser AI agent can call c0mpute tools directly —{" "}
            <code>c0mpute_list_plugins</code>,{" "}
            <code>c0mpute_network_status</code>,{" "}
            <code>c0mpute_latest_release</code>,{" "}
            <code>c0mpute_install_command</code> — no scraping.
          </p>
          <p className="mt-1 text-[var(--color-dim)]">
            → <Link href="/agents">buying compute as an agent</Link> ·{" "}
            <Link href="/docs#agents">docs / ai agents</Link>
          </p>
        </div>
      </section>

      <section className="space-y-3 rule pt-8">
        <p className="comment">// next</p>
        <ul className="space-y-1 text-sm">
          <li>→ <Link href="/getting-started">getting-started</Link></li>
          <li>→ <Link href="/protocol">protocol</Link></li>
          <li>→ <Link href="/providers">providers</Link></li>
          <li>→ <Link href="/agents">agents</Link></li>
          <li>→ <Link href="/plugins">plugins</Link></li>
          <li>→ <Link href="/docs">docs</Link></li>
          <li>→ <a href="https://github.com/profullstack/c0mpute">github.com/profullstack/c0mpute</a></li>
        </ul>
      </section>
    </div>
  );
}

function Step({ n, title, body }: { n: string; title: string; body: string }) {
  return (
    <div className="border border-[var(--color-rule)] bg-[var(--color-card)] rounded p-4 space-y-1">
      <p className="text-xs text-[var(--color-dim)]">{n}.</p>
      <p className="text-sm accent">{title}</p>
      <p className="text-xs text-[var(--color-dim)] leading-6">{body}</p>
    </div>
  );
}
