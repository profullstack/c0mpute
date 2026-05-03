import Link from "next/link";

export default function HomePage() {
  return (
    <main className="min-h-screen p-8 max-w-3xl mx-auto space-y-12">
      <header className="space-y-3 pt-12">
        <p className="text-sm uppercase tracking-widest text-white/40">
          depin.quest / video
        </p>
        <h1 className="text-5xl font-bold text-primary leading-tight">Quest</h1>
        <p className="text-xl text-white/80 max-w-prose">
          Decentralized video transcoding & hosting. Anyone runs a node, earns
          for transcoding and serving video, and customers pay 50–80% less than
          Mux or CloudFront.
        </p>
      </header>

      <section className="space-y-3">
        <h2 className="text-2xl font-semibold">Run a node</h2>
        <p className="text-white/70">
          Single static Rust binary <code>depin</code>. Self-installs,
          self-upgrades, self-heals. Today the only product line is{" "}
          <code>depin video</code>.
        </p>
        <pre className="bg-white/5 border border-white/10 p-4 rounded-lg text-sm overflow-x-auto">
          <code>{`curl -fsSL https://depin.quest/video/install.sh | sh
depin video start --roles storage,transcode,gateway`}</code>
        </pre>
        <Link
          href="/install"
          className="inline-block text-primary underline underline-offset-4 hover:opacity-80"
        >
          Full install instructions →
        </Link>
      </section>

      <section className="grid gap-4 sm:grid-cols-3 text-sm">
        <Card
          title="Transcode"
          body="FFmpeg + NVENC/QSV/AMF/AV1. ~$0.30/hr for four 1080p renditions."
        />
        <Card
          title="Store"
          body="Reed-Solomon erasure coding. $0.005/GB-month, 99.9% durability target."
        />
        <Card
          title="Stream"
          body="HLS over libp2p chunk transport. Drop-in <iframe> embed."
        />
      </section>

      <section className="space-y-3 border-t border-white/10 pt-8">
        <h2 className="text-2xl font-semibold">For builders</h2>
        <ul className="space-y-2 text-white/80">
          <li>
            <Link href="/docs" className="text-primary hover:opacity-80">
              Documentation
            </Link>{" "}
            — API reference, embed snippets, self-hosting guide.
          </li>
          <li>
            <Link href="/app" className="text-primary hover:opacity-80">
              Dashboard
            </Link>{" "}
            — manage videos, monitor nodes, top up balance, withdraw earnings.
          </li>
          <li>
            <a
              href="https://github.com/depinquest/quest"
              className="text-primary hover:opacity-80"
            >
              github.com/depinquest/quest
            </a>{" "}
            — Apache-2.0, DCO sign-off, contributions welcome.
          </li>
        </ul>
      </section>

      <footer className="text-xs text-white/40 pt-12 pb-8">
        Quest is one product line under the depin.quest brand. Future lines
        live alongside it under <code>/storage</code>, <code>/compute</code>, etc.
      </footer>
    </main>
  );
}

function Card({ title, body }: { title: string; body: string }) {
  return (
    <div className="border border-white/10 bg-white/[0.03] p-4 rounded-lg space-y-2">
      <h3 className="text-base font-semibold text-primary">{title}</h3>
      <p className="text-white/70 leading-snug">{body}</p>
    </div>
  );
}
