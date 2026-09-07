import Link from "next/link";
import { CodeBlock } from "@/components/CodeBlock";

export default function HomePage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-12">
      <section className="space-y-4">
        <h1 className="text-3xl font-bold accent">The Open Compute Network</h1>
        <p className="text-[var(--color-fg)] leading-relaxed">
          Run jobs anywhere. Sell unused compute. Pay only for verified work.
        </p>
        <p className="text-sm text-[var(--color-dim)] leading-relaxed">
          CPUs, GPUs, storage, bandwidth and specialized accelerators — from
          PCs, servers and data centers — available through one open protocol,
          to humans and AI agents alike.
        </p>
      </section>

      <section className="space-y-3">
        <p className="comment">// install — Linux, macOS (x86_64 / aarch64)</p>
        <CodeBlock>{`$ curl -fsSL https://c0mpute.com/install.sh | sh`}</CodeBlock>
        <p className="text-xs text-[var(--color-dim)]">
          → <Link href="/getting-started">submit a job</Link> ·{" "}
          <Link href="/providers">become a provider</Link> ·{" "}
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
$ c0mpute infernet run prompts.jsonl --model qwen --max-price 0.10`}</CodeBlock>
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
