import Link from "next/link";
import { CodeBlock } from "@/components/CodeBlock";

export default function HomePage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-12">
      <section className="space-y-4">
        <h1 className="text-3xl font-bold accent">c0mpute</h1>
        <p className="comment">// decentralized compute network · cli-first</p>
        <p className="text-[var(--color-fg)] leading-relaxed">
          Pay workers with spare GPUs to run your jobs. Run your own GPU and
          earn. Three modules out of the box:{" "}
          <span className="accent">transcode</span> (FFmpeg),{" "}
          <span className="accent">coinpay</span> (DID + escrow),{" "}
          <span className="accent">infernet</span> (AI inference).
        </p>
      </section>

      <section className="space-y-3">
        <p className="comment">// install — Linux, macOS (x86_64 / aarch64)</p>
        <CodeBlock>{`$ curl -fsSL https://c0mpute.com/install.sh | sh`}</CodeBlock>
      </section>

      <section className="space-y-3">
        <p className="comment">// run a worker</p>
        <CodeBlock>{`$ c0mpute coinpay did create --role worker
$ c0mpute worker register
$ c0mpute worker start --gpu`}</CodeBlock>
      </section>

      <section className="space-y-3">
        <p className="comment">// submit a job</p>
        <CodeBlock>{`$ c0mpute transcode submit input.mov --preset hls
$ c0mpute infernet run prompts.jsonl --model qwen`}</CodeBlock>
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

      <section className="space-y-3 rule pt-8">
        <p className="comment">// next</p>
        <ul className="space-y-1 text-sm">
          <li>→ <Link href="/getting-started">getting-started</Link></li>
          <li>→ <Link href="/docs">docs</Link></li>
          <li>→ <Link href="/plugins">plugins</Link></li>
          <li>→ <a href="https://github.com/profullstack/c0mpute">github.com/profullstack/c0mpute</a></li>
        </ul>
      </section>
    </div>
  );
}
