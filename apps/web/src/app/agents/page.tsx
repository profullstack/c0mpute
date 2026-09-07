import Link from "next/link";
import { CodeBlock } from "@/components/CodeBlock";

export const metadata = {
  title: "agents — c0mpute",
  description:
    "AI agents should be able to purchase compute as easily as humans call an API. c0mpute exposes CLI structured output, a local daemon API, MCP tools, WebMCP, machine-readable job manifests, hard budget caps and signed receipts.",
  alternates: { canonical: "https://c0mpute.com/agents" },
};

export default function AgentsPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">agents</h1>
        <p className="comment">// buying compute without a human in the loop</p>
      </header>

      <section className="space-y-3 text-sm leading-7">
        <p className="text-[var(--color-fg)]">
          AI agents should be able to purchase compute as easily as humans
          call an API.
        </p>
        <p>
          That is not a slogan about a chat integration. An autonomous buyer
          needs four things a conventional cloud does not give it: a
          machine-readable way to <em>describe work</em> rather than pick a
          machine, a hard spending limit it cannot exceed, a way to tell
          whether it got what it paid for, and an identity it can create
          itself. c0mpute is built around all four, because the protocol was
          designed for a caller that is not a person.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ the job is the API ]</h2>
        <p>
          There is no SDK to learn and no console to click through. An agent
          emits a signed JSON manifest describing constraints, and the network
          answers with signed offers.
        </p>
        <CodeBlock>{`{
  "workload":     { "type": "infernet.inference", "version": ">=1 <2" },
  "requirements": { "gpu": { "count": 1, "vramGiB": { "min": 24 } },
                    "trustTier": "standard" },
  "execution":    { "timeoutSeconds": 120, "isolation": "container",
                    "network": false },
  "economics":    { "maxPrice": { "amount": "0.10", "currency": "USD" },
                    "settlement": "coinpay", "escrow": true },
  "validation":   { "level": "spotcheck", "spotcheckPercent": 5 },
  "deadline":     "2026-09-06T16:10:00.000Z"
}`}</CodeBlock>
        <p className="text-[var(--color-dim)]">
          The job&apos;s id is the content hash of that manifest, so an agent
          can name a job without a registry assigning it an id, and two agents
          holding the same manifest always agree on what to call it.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ budget caps are structural ]</h2>
        <p>
          <code>maxPrice</code> is part of the signed manifest, and an offer
          above it is not eligible — not discouraged, not warned about.
          Amounts are decimal strings compared by value, so an agent cannot be
          caught by a float rounding the wrong way or by a string comparison
          ranking <code>&quot;0.9&quot;</code> under{" "}
          <code>&quot;0.10&quot;</code>.
        </p>
        <p className="text-[var(--color-dim)]">
          Every money-spending tool takes an explicit budget constraint. An
          agent that has authority to spend $2 has it because something signed
          a manifest saying $2.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ verifiable outcomes ]</h2>
        <p>
          The hard part of autonomous purchasing is not paying — it is knowing
          whether you got what you paid for. Every completed job produces a
          signed receipt naming the job, the offer that priced it, and the
          input, output and runtime by hash.
        </p>
        <p>
          An agent can check that the amount charged is the amount quoted, and
          a later audit can reproduce the result from the same input through
          the same pinned runtime. Nothing has to be taken on trust, and
          nothing has to be looked up from a service that might be gone.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ surfaces ]</h2>
        <table className="w-full text-left border-collapse text-xs">
          <tbody className="divide-y divide-[var(--color-rule)]">
            <Row
              k="CLI"
              v="every command takes --json for structured output; exit codes are stable. The path of least resistance for an agent that already has a shell."
            />
            <Row
              k="local daemon"
              v="a loopback socket so tools share one persistent network session instead of spawning a peer per command. In progress."
            />
            <Row
              k="MCP"
              v="c0mpute.network.status, providers.search, jobs.submit, jobs.status, jobs.cancel, jobs.receipt, rates.estimate. In progress."
            />
            <Row
              k="WebMCP"
              v="live on this site today: c0mpute_list_plugins, c0mpute_network_status, c0mpute_latest_release, c0mpute_install_command — no scraping."
            />
            <Row
              k="managed gateway"
              v="OpenAI-compatible endpoints for callers that want an API key instead of a keypair. A client of the network, not the network."
            />
          </tbody>
        </table>
        <p className="text-xs text-[var(--color-dim)]">
          Marked honestly: WebMCP works now, the daemon and MCP server are
          Phase 2. See <Link href="/protocol">what is built</Link>.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ identity without signup ]</h2>
        <p>
          An agent generates its own keypair locally and is immediately a peer
          — no account, no API key issued by anyone, no email confirmation
          loop that an autonomous process cannot complete. The DID{" "}
          <em>is</em> the public key, so counterparties verify its signatures
          with no lookup.
        </p>
        <CodeBlock>{`$ c0mpute worker register --json
{ "did": "did:c0mpute:z6Mksw…" }`}</CodeBlock>
      </section>

      <section className="space-y-2 rule pt-8 text-xs text-[var(--color-dim)]">
        <p>
          → <Link href="/protocol">protocol</Link> ·{" "}
          <Link href="/docs#agents">docs / ai agents</Link> ·{" "}
          <Link href="/pricing">pricing</Link>
        </p>
      </section>
    </div>
  );
}

function Row({ k, v }: { k: string; v: string }) {
  return (
    <tr>
      <td className="py-2 pr-6 align-top accent whitespace-nowrap">{k}</td>
      <td className="py-2 text-[var(--color-dim)] leading-6">{v}</td>
    </tr>
  );
}
