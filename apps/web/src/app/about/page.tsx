import Link from "next/link";

export const metadata = {
  title: "about — c0mpute",
  description: "c0mpute is an open-source, CLI-first decentralized compute network. MIT-licensed. Built by Profullstack, Inc.",
  alternates: { canonical: "https://c0mpute.com/about" },
};

export default function AboutPage() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">about</h1>
        <p className="comment">// who's behind this and why</p>
      </header>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ what is c0mpute ]</h2>
        <p>
          c0mpute is an open-source, CLI-first marketplace for GPU compute.
          Buyers pay workers with spare GPUs to run jobs — video transcoding,
          AI inference, and more. Workers earn by registering their hardware
          and accepting work. No central backend, no middleman.
        </p>
        <p>
          The network is built on three modules:{" "}
          <code>transcode</code> (FFmpeg-based GPU video encoding),{" "}
          <code>coinpay</code> (DID identity and escrow payments), and{" "}
          <code>infernet</code> (AI LLM inference). All MIT-licensed.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ who builds it ]</h2>
        <p>
          c0mpute is developed and maintained by{" "}
          <a href="https://profullstack.com" target="_blank" rel="noopener noreferrer">
            Profullstack, Inc.
          </a>
          {" "}— a small software company focused on open-source developer tooling.
        </p>
        <ul className="space-y-2 mt-2">
          <li>
            <span className="accent">Anthony Ettinger</span>{" "}
            <span className="text-[var(--color-dim)]">— founder &amp; lead engineer</span>
            {" · "}
            <a href="https://github.com/chovy" target="_blank" rel="noopener noreferrer">github</a>
          </li>
        </ul>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ design philosophy ]</h2>
        <ul className="space-y-1 list-none">
          <li><span className="accent">cli-first</span> — the terminal is the UI. No dashboard required.</li>
          <li><span className="accent">no central backend</span> — workers and buyers connect peer-to-peer via DIDs.</li>
          <li><span className="accent">open source</span> — MIT license. Fork it, run your own network, contribute upstream.</li>
          <li><span className="accent">modular</span> — install only what you need: transcode, coinpay, or infernet.</li>
        </ul>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ status ]</h2>
        <p>
          c0mpute is pre-mainnet. The CLI, modules, and protocol design are
          published and installable today. The live network is in early
          testing — see{" "}
          <Link href="/status" className="hover:text-[var(--color-accent)]">network status</Link>{" "}
          for current metrics.
        </p>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ get involved ]</h2>
        <ul className="space-y-2">
          <li>
            <a href="https://github.com/profullstack/c0mpute" target="_blank" rel="noopener noreferrer">
              github.com/profullstack/c0mpute
            </a>{" "}
            <span className="text-[var(--color-dim)]">— source code, issues, design proposals (DIPs)</span>
          </li>
          <li>
            <Link href="/getting-started" className="hover:text-[var(--color-accent)]">getting-started</Link>{" "}
            <span className="text-[var(--color-dim)]">— install the CLI and run your first job</span>
          </li>
          <li>
            <Link href="/contact" className="hover:text-[var(--color-accent)]">contact</Link>{" "}
            <span className="text-[var(--color-dim)]">— hello@c0mpute.com for general inquiries</span>
          </li>
        </ul>
      </section>
    </div>
  );
}
