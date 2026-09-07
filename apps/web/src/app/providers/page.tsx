import Link from "next/link";
import { CodeBlock } from "@/components/CodeBlock";

export const metadata = {
  title: "providers — c0mpute",
  description:
    "Turn idle hardware into a c0mpute provider. Personal PCs, homelabs, GPU farms, VPS providers, data centers and private enterprise capacity earn on the same open protocol. See exactly what your node advertises before it starts.",
  alternates: { canonical: "https://c0mpute.com/providers" },
};

export default function ProvidersPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">providers</h1>
        <p className="comment">// turn idle hardware into a c0mpute provider</p>
      </header>

      <section className="space-y-3 text-sm leading-7">
        <p>
          One protocol, whatever you are running it on. A gaming GPU idle
          overnight and a rack of A100s advertise capacity the same way, quote
          the same way, and get paid the same way. Nobody approves you.
        </p>
        <CodeBlock>{`$ curl -fsSL https://c0mpute.com/install.sh | sh
$ c0mpute worker start`}</CodeBlock>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ who runs one ]</h2>
        <table className="w-full text-left border-collapse text-xs">
          <tbody className="divide-y divide-[var(--color-rule)]">
            <Row k="personal PC" v="a desktop or laptop that is idle some of the time. No inbound ports, no static IP, no commitment." />
            <Row k="homelab" v="a always-on box or two. Often the best latency-to-price ratio on the network." />
            <Row k="GPU farm" v="mining or render capacity looking for a second demand curve." />
            <Row k="hosting / VPS" v="unsold capacity, advertised without building a marketplace of your own." />
            <Row k="data center" v="professional operators serving the trust tiers that need an SLA." />
            <Row k="private capacity" v="an enterprise fleet on a private overlay, using the same protocol and settling internally." />
          </tbody>
        </table>
      </section>

      {/* The PRD asks for this explicitly, and it is the right instinct:
          nobody should have to run the binary to find out what it publishes
          about their machine. */}
      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ what your node advertises ]</h2>
        <p>
          Before it starts, not after. This is the whole of what goes on the
          wire — a signed advert, re-published every few minutes:
        </p>
        <CodeBlock>{`{
  "type": "c0mpute.provider.advert/v1",
  "signer": "did:c0mpute:z6Mksw…",
  "payload": {
    "sequence": 8472,
    "issuedAt":  "2026-09-06T23:50:00.000Z",
    "expiresAt": "2026-09-06T23:59:00.000Z",
    "capabilities": {
      "cpu":        { "arch": "x86_64", "cores": 32 },
      "memoryGiB":  128,
      "gpus":       [ { "vendor": "nvidia", "family": "ada",
                        "vramGiB": 24, "features": ["cuda", "nvenc"] } ],
      "storageGiB": 1200,
      "workloads":  ["infernet.inference", "transcode.ffmpeg"]
    },
    "trust": { "tier": "standard" }
  },
  "sig": "z5p6RP…"
}`}</CodeBlock>
        <p className="text-[var(--color-dim)]">
          Coarse hardware classes, not an exact model. No hostname, no
          location, no process list, no OS fingerprint. Region, country and
          ASN exist as fields and are <em>opt-in</em> — leaving them out is a
          supported way to run a provider, and the only consequence is that a
          buyer who requires a region cannot match you.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ you choose the work ]</h2>
        <p>
          Advertising a workload is an acceptance policy, not just a statement
          of capability. Taking Whisper transcription and refusing arbitrary
          containers is the normal posture for a machine you also use for
          other things.
        </p>
        <p className="text-[var(--color-dim)]">
          Untrusted workloads run rootless and containerized, with no host
          filesystem access, CPU/RAM/disk quotas, hard timeouts and{" "}
          <strong className="text-[var(--color-fg)]">no network access
          unless the job asked for it and you allowed it</strong>. Typed
          plugins come first; generic container jobs only land behind stronger
          isolation.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ behind NAT is fine ]</h2>
        <p>
          A residential provider must be able to earn without opening inbound
          ports, so relay and outbound-only paths are part of the design
          rather than a workaround. Inbound reachability is not a requirement
          for basic participation.
        </p>
        <p className="text-xs text-[var(--color-dim)]">
          Relay paths are Phase 2 work — see{" "}
          <Link href="/protocol">what is built</Link>.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ what you earn ]</h2>
        <p>
          You set your rates. A buyer sets a cap. When you quote under it and
          the work validates, settlement releases and both sides sign a
          receipt. There is{" "}
          <strong className="text-[var(--color-fg)]">no protocol fee</strong>{" "}
          — the amount agreed is the amount transferred.
        </p>
        <p className="text-[var(--color-dim)]">
          Your record is portable. Reputation is derived from signed receipts
          you hold, not from a score in a database we control, so it survives
          us and cannot be quietly edited. Failures are receipts too — a
          network where only successes are signed measures nothing.
        </p>
        <p className="text-xs text-[var(--color-dim)]">
          → <Link href="/pricing">pricing and units</Link>
        </p>
      </section>

      <section className="space-y-3 rule pt-8 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ start ]</h2>
        <CodeBlock>{`$ c0mpute login                 # optional: ties the node to your accounts
$ c0mpute worker register       # mints your DID locally
$ c0mpute worker start --gpu
$ c0mpute worker stats`}</CodeBlock>
        <p className="text-xs text-[var(--color-dim)]">
          → <Link href="/getting-started">getting-started</Link> ·{" "}
          <Link href="/plugins">what you can run</Link> ·{" "}
          <Link href="/protocol">the protocol</Link>
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
