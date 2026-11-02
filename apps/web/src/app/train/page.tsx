import Link from "next/link";

export const metadata = {
  title: "train your own AI agent — c0mpute",
  description:
    "Fine-tune a language model on your own writing on someone else's machine, watch it train live, then chat with it on the network with answers cited from pit.world search. LoRA on CPU or GPU workers: infernet.text.train and infernet.text.infer.",
  alternates: { canonical: "https://c0mpute.com/train" },
};

const Code = ({ children }: { children: string }) => (
  <pre className="overflow-x-auto border border-[var(--color-border,#333)] p-4 text-xs leading-6">
    <code>{children}</code>
  </pre>
);

export default function TrainPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">train your own AI agent</h1>
        <p className="comment">// fine-tune a model on your corpus on a c0mpute worker, watch it train, then chat with it on the network</p>
      </header>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ what it is ]</h2>
        <p>
          Give c0mpute a folder of writing (blog posts, docs, support answers, a newsletter archive) and a small
          open model. A worker on the network fine-tunes it with LoRA while you watch the loss fall, and hands back
          an adapter of a few megabytes. Then ask that adapter questions: another worker answers, with the facts
          pulled from <a href="https://pit.world">pit.world</a> search and cited by URL. No GPU, no cloud account,
          no Python on your machine.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ 1. train ]</h2>
        <Code>{`curl -fsSL https://c0mpute.com/install.sh | sh        # c0mpute 0.2.51+

cat > job.json <<'EOF'
{ "name": "my-agent", "kind": "lora",
  "base_model": "HuggingFaceTB/SmolLM2-135M-Instruct",
  "system_prompt": "You are a concise, practical business strategist.",
  "hyperparams": { "max_steps": 200, "batch_size": 2, "seq_len": 256 } }
EOF

# corpus.jsonl: one {"title": "...", "text": "..."} per line
c0mpute infernet text train --job job.json --dataset corpus.jsonl --output ./adapter --yes`}</Code>
        <p>What you see while it trains:</p>
        <Code>{`looking for a worker for my-agent ...
training HuggingFaceTB/SmolLM2-135M-Instruct on cpu (48 docs, 6 steps)
eval @0: {"perplexity":6.22}
[########################] step 6/6  loss 1.93  92.5 tok/s
eval @6: {"perplexity":6.14}
adapter sha256:b8533a34...  ./adapter  (worker 12D3KooWSB7L...)`}</Code>
        <p>
          Without <code>--yes</code> it prints what would be sent and stops. The adapter comes back only if its
          hash matches the worker&apos;s receipt.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ 2. chat ]</h2>
        <Code>{`c0mpute infernet text chat --adapter ./adapter "How should I price my SaaS?"
To price your SaaS product effectively, consider the following ...
  [1] https://blog.hubspot.com/sales/price-saas-product
  [2] https://www.cloudzero.com/blog/saas-pricing/
  [3] https://stripe.com/resources/more/software-pricing-models-and-strategies-for-saas-businesses`}</Code>
        <p>
          Your machine searches pit.world for the question and sends it, with the top pages as background and your
          adapter, to a worker. The worker caches the adapter by hash, answers, and its receipt carries the
          answer&apos;s hash. A small model learns your voice, not every fact, so the facts come from search.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ 3. or run a worker ]</h2>
        <Code>{`curl -fsSL https://c0mpute.com/install.sh | sh
npm i -g @infernetprotocol/cli                      # infernet 0.1.80+
infernet model runtime setup --peft --yes           # PyTorch + transformers + peft, ~1 GB
c0mpute infernet text serve                         # take training and chat jobs`}</Code>
        <p>
          A worker trains one job at a time, answers chat requests between them, and advertises only what its
          Infernet can run (CPU or CUDA, RAM bucket). Earnings follow the bid on each receipt.{" "}
          <Link href="/nodes">See who is online.</Link>
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ honest notes ]</h2>
        <ul className="space-y-1 list-none">
          <li><span className="accent">privacy</span> — the worker sees the corpus it trains on and the question it answers. Use a worker you trust (<code>--worker &lt;peer-id&gt;</code>) for anything private.</li>
          <li><span className="accent">reachability</span> — the worker fetches your corpus and sends progress to a small token-gated server on your machine. Behind a firewall, pass <code>--bind 0.0.0.0:&lt;port&gt; --public-url http://&lt;reachable ip&gt;:&lt;port&gt;</code>.</li>
          <li><span className="accent">size</span> — CPU workers make 135M to 1.5B models practical. Bigger bases need a CUDA worker (<code>--gpu</code>).</li>
          <li><span className="accent">early</span> — few text workers are online yet, so a job may wait for one. Payment settlement beyond the bid on the receipt is not built.</li>
        </ul>
        <p>
          Spec: <a href="https://github.com/profullstack/c0mpute/blob/master/dips/0027-infernet-text-train.md">DIP 0027</a>
          {" · "}
          <a href="https://github.com/profullstack/c0mpute/tree/master/examples/business-strategist-agent">example: business-strategist-agent</a>
          {" · "}
          <a href="https://infernetprotocol.com">Infernet</a>
        </p>
      </section>
    </div>
  );
}
