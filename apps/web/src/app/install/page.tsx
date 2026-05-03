import Link from "next/link";

export const metadata = {
  title: "Install depin — Quest",
};

export default function InstallPage() {
  return (
    <main className="min-h-screen p-8 max-w-3xl mx-auto space-y-10">
      <header className="space-y-2 pt-8">
        <Link href="/" className="text-sm text-white/50 hover:text-white/80">
          ← Quest
        </Link>
        <h1 className="text-4xl font-bold text-primary">
          Install <code>depin</code>
        </h1>
        <p className="text-white/70">
          One command. No sudo. Linux, macOS (x86_64 / aarch64). The{" "}
          <code>depin</code> binary covers every product line on{" "}
          depin.quest — today that's <code>depin video</code> (Quest).
        </p>
      </header>

      <section className="space-y-3">
        <h2 className="text-xl font-semibold">Quick install</h2>
        <pre className="bg-white/5 border border-white/10 p-4 rounded-lg text-sm overflow-x-auto">
          <code>curl -fsSL https://depin.quest/video/install.sh | sh</code>
        </pre>
        <p className="text-sm text-white/60">
          The script downloads <code>depin-&lt;os&gt;-&lt;arch&gt;.tar.gz</code>{" "}
          from <code>depin.quest/video/releases/latest</code>, verifies the
          minisign signature against an embedded public key, installs to{" "}
          <code>~/.depin/bin/depin</code>, and runs <code>depin video doctor</code>.
        </p>
      </section>

      <section className="space-y-3">
        <h2 className="text-xl font-semibold">Start a node</h2>
        <pre className="bg-white/5 border border-white/10 p-4 rounded-lg text-sm overflow-x-auto whitespace-pre-wrap">
          <code>{`# Storage + gateway only (no GPU required)
depin video start --roles storage,gateway

# All-in including GPU transcoding
depin video start --roles storage,transcode,gateway,verifier --storage 500GB --gpu`}</code>
        </pre>
      </section>

      <section className="space-y-3">
        <h2 className="text-xl font-semibold">Verify your install</h2>
        <pre className="bg-white/5 border border-white/10 p-4 rounded-lg text-sm overflow-x-auto whitespace-pre-wrap">
          <code>{`depin video doctor          # run diagnostics
depin video doctor --fix    # auto-apply known remediations
depin version`}</code>
        </pre>
      </section>

      <section className="space-y-3 border-t border-white/10 pt-8">
        <h2 className="text-xl font-semibold">Manual download</h2>
        <p className="text-white/70 text-sm">
          If you'd rather not pipe-to-shell, grab the tarball + signature
          directly:
        </p>
        <ul className="text-sm space-y-1 text-white/80">
          <li>
            <code>
              https://depin.quest/video/releases/latest/depin-linux-x86_64.tar.gz
            </code>
          </li>
          <li>
            <code>
              https://depin.quest/video/releases/latest/depin-linux-x86_64.tar.gz.minisig
            </code>
          </li>
          <li>
            <code>
              https://depin.quest/video/releases/latest/depin-darwin-aarch64.tar.gz
            </code>
          </li>
        </ul>
      </section>
    </main>
  );
}
