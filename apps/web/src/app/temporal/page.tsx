export const metadata = {
  title: "temporal policies (liquid networks) — c0mpute",
  description:
    "Train and serve CfC/LTC liquid neural networks and GRU baselines on c0mpute workers: infernet.temporal.train and infernet.temporal.infer on the existing job and receipt protocol.",
  alternates: { canonical: "https://c0mpute.com/temporal" },
};

const Code = ({ children }: { children: string }) => (
  <pre className="overflow-x-auto border border-[var(--color-border,#333)] p-4 text-xs leading-6">
    <code>{children}</code>
  </pre>
);

export default function TemporalPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">temporal policies</h1>
        <p className="comment">// liquid neural networks (CfC, LTC) and a GRU baseline, trained and served on c0mpute workers</p>
      </header>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ what it is ]</h2>
        <p>
          Small temporal policies that learn a desktop task (drafting an email reply, composing a Reddit post,
          reviewing LinkedIn notifications) from consented, redacted demonstrations recorded in{" "}
          <a href="https://w3bs.org/training/liquid-agent">W3BS Training</a>, then propose the next UI action.{" "}
          <a href="https://infernetprotocol.com/docs/liquid-agent">Infernet</a> runs the models; c0mpute finds a
          worker, moves the job and carries the receipt. Same offer, bid, accept and receipt protocol as every other
          workload.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ run a temporal worker ]</h2>
        <Code>{`curl -fsSL https://c0mpute.com/install.sh | sh     # c0mpute 0.2.34+
npm i -g @infernetprotocol/cli                     # infernet 0.1.66+
infernet model runtime setup --yes                 # PyTorch + ncps, your choice
c0mpute infernet temporal serve --train            # accept training jobs
c0mpute infernet temporal serve --model sha256:<hash>   # serve a model`}</Code>
        <p>
          A worker advertises only what its Infernet reports: runtime version, CPU / CUDA / Apple GPU, RAM bucket
          and the model hashes it serves. It runs one training job at a time and never bids on a job without the
          user&apos;s upload approval.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ train on a trusted worker ]</h2>
        <Code>{`c0mpute infernet temporal train --dataset bundle.json --output ./model \\
  --family cfc --baseline gru --worker <peer-id> --max-price 0.50 --yes`}</Code>
        <p>
          Without <code>--yes</code> it prints what would be sent and stops. <code>--worker</code> limits bids to
          peers you trust. The model comes back only if its weights hash matches the worker&apos;s receipt. A
          liquid family is picked only when it beats the GRU baseline.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ sticky, encrypted inference ]</h2>
        <Code>{`c0mpute infernet temporal predict --session s1 --model sha256:<hash> --input step.json`}</Code>
        <p>
          A session opens on one worker and stays there, because that worker holds its hidden state. Calls are
          end-to-end encrypted (X25519, HKDF, AES-256-GCM) between you and that worker, answered only for the peer
          that opened the session, run exactly once, and rejected if a step is late or repeated.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ honest notes ]</h2>
        <ul className="space-y-1 list-none">
          <li><span className="accent">privacy</span> — a worker sees the redacted data it trains on. Encryption in transit does not change that. Local training is the default.</li>
          <li><span className="accent">results</span> — on synthetic mock traces GRU beat CfC (0.948 vs 0.869 episode success), so GRU is the default until real data says otherwise.</li>
          <li><span className="accent">not yet</span> — moving a live session to another worker; settlement for these receipts.</li>
        </ul>
        <p>
          Spec: <a href="https://github.com/profullstack/c0mpute/blob/master/dips/0026-infernet-temporal-workloads.md">DIP 0026</a>
          {" · "}
          <a href="https://infernetprotocol.com/docs/liquid-agent">Infernet docs</a>
          {" · "}
          <a href="https://w3bs.org/training/liquid-agent">W3BS: teach an agent</a>
        </p>
      </section>
    </div>
  );
}
