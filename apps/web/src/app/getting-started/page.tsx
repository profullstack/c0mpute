import { CodeBlock } from "@/components/CodeBlock";

export const metadata = {
  title: "getting-started — c0mpute",
  description: "Get up and running with c0mpute in minutes. Install the CLI, create your CoinPay DID, register as a worker, and submit your first GPU compute job.",
  alternates: { canonical: "https://c0mpute.com/getting-started" },
};

export default function GettingStartedPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">getting-started</h1>
        <p className="comment">// install → identity → register → run</p>
      </header>

      <Section number="1" label="install the cli stack">
        <CodeBlock>{`$ curl -fsSL https://c0mpute.com/install.sh | sh`}</CodeBlock>
        <p className="text-sm text-[var(--color-dim)]">
          Installs three binaries into <code>~/.c0mpute/bin</code>:{" "}
          <span className="accent">c0mpute</span>,{" "}
          <span className="accent">coinpay</span>,{" "}
          <span className="accent">infernet</span>. No sudo. The worker
          self-upgrades every 5 minutes.
        </p>
      </Section>

      <Section number="2" label="sign in to coinpay">
        <CodeBlock>{`$ c0mpute coinpay login`}</CodeBlock>
        <p className="text-sm text-[var(--color-dim)]">
          Links your payable DID to your coinpay account. The DID anchors your
          reputation, payments, and signed receipts —{" "}
          <code>c0mpute worker register</code> sets it up automatically (uses
          your existing DID or creates one).
        </p>
      </Section>

      <Section number="3" label="run a worker">
        <CodeBlock>{`$ c0mpute worker register
$ c0mpute worker start --gpu`}</CodeBlock>
      </Section>

      <Section number="4" label="submit a job">
        <CodeBlock>{`$ c0mpute transcode submit input.mov --preset hls --max-price 1.25
$ c0mpute infernet run prompts.jsonl --model qwen
$ c0mpute job status <job-id>`}</CodeBlock>
      </Section>

      <Section number="5" label="interactive tui">
        <CodeBlock>{`$ c0mpute tui`}</CodeBlock>
        <p className="text-sm text-[var(--color-dim)]">
          Live worker / job dashboard, terminal-native (react-blessed).
        </p>
      </Section>

      <Section number="6" label="upgrade & uninstall">
        <CodeBlock>{`$ c0mpute update              # check for + apply new release
$ c0mpute upgrade --check     # alias; just check, don't apply
$ c0mpute uninstall           # remove c0mpute
$ c0mpute uninstall --all     # remove c0mpute, coinpay, infernet
$ c0mpute uninstall --purge   # also remove ~/.config/c0mpute`}</CodeBlock>
      </Section>

      <Section number="7" label="check the stack">
        <CodeBlock>{`$ c0mpute doctor`}</CodeBlock>
      </Section>
    </div>
  );
}

function Section({
  number,
  label,
  children,
}: {
  number: string;
  label: string;
  children: React.ReactNode;
}) {
  return (
    <section className="space-y-3">
      <h2 className="text-lg">
        <span className="accent">[{number}]</span>{" "}
        <span className="text-[var(--color-fg)]">{label}</span>
      </h2>
      <div className="space-y-2 pl-5">{children}</div>
    </section>
  );
}
