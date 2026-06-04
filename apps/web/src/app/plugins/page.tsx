import Link from "next/link";

import { CodeBlock } from "@/components/CodeBlock";
import {
  loadAllPlugins,
  tagline,
  installCommand,
  type PluginManifest,
} from "@/lib/plugins";

export const metadata = {
  title: "plugins — c0mpute",
  description: "c0mpute plugin ecosystem: transcode (FFmpeg GPU video encoding), coinpay (DID identity + escrow payments), and infernet (AI LLM inference). MIT-licensed.",
  alternates: { canonical: "https://c0mpute.com/plugins" },
};

export default function PluginsPage() {
  const plugins = loadAllPlugins();

  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">plugins</h1>
        <p className="comment">
          // installable workload + service plugins for the c0mpute CLI
        </p>
      </header>

      <section className="space-y-2 text-sm leading-6">
        <p>
          Plugins extend the c0mpute CLI with new workload types or
          services. The three v1 plugins are pre-installed by{" "}
          <code>curl https://c0mpute.com/install.sh | sh</code>.
        </p>
        <p>
          Third-party plugins install with{" "}
          <code>c0mpute plugin install &lt;url&gt;</code> where{" "}
          <code>&lt;url&gt;</code> points at the plugin's signed{" "}
          <code>install.sh</code>. See{" "}
          <a href="https://github.com/profullstack/c0mpute/blob/master/dips/0006-module-model.md">
            DIP-0006
          </a>{" "}
          for the model.
        </p>
      </section>

      <section className="space-y-4">
        {plugins.map((p) => (
          <PluginCard key={p.id} p={p} />
        ))}
      </section>

      <section className="rule pt-8 text-sm text-[var(--color-dim)]">
        <p>
          Want to publish a plugin? For now, open a PR adding{" "}
          <code>plugins/&lt;your-id&gt;/module.toml</code> on{" "}
          <a href="https://github.com/profullstack/c0mpute">
            github.com/profullstack/c0mpute
          </a>
          . The marketplace UI here renders from those manifests at build
          time. A submission API lands once we have signing in place.
        </p>
      </section>

      <p className="text-xs text-[var(--color-dim)]">
        → <Link href="/docs">docs</Link> ·{" "}
        <Link href="/getting-started">getting-started</Link>
      </p>
    </div>
  );
}

function PluginCard({ p }: { p: PluginManifest }) {
  const dispatchLabel =
    p.dispatch?.mode === "in-process"
      ? "in-process"
      : p.dispatch?.mode === "container"
        ? "container"
        : "subprocess";

  return (
    <article className="border border-[var(--color-rule)] bg-[var(--color-card)] rounded p-5 space-y-3">
      <header className="flex items-baseline justify-between gap-3">
        <h2 className="text-lg accent">
          <span className="text-[var(--color-dim)]">[</span>
          {p.id}
          <span className="text-[var(--color-dim)]">]</span>{" "}
          <span className="text-[var(--color-fg)]">{p.name}</span>
        </h2>
        <span className="text-xs text-[var(--color-dim)]">
          v{p.version} · {p.kind} · {dispatchLabel}
        </span>
      </header>

      <p className="text-sm text-[var(--color-fg)] leading-snug">
        {tagline(p)}
      </p>

      {p.keywords && p.keywords.length > 0 && (
        <p className="text-xs text-[var(--color-dim)]">
          {p.keywords.map((k) => `#${k}`).join("  ")}
        </p>
      )}

      {p.dispatch?.mode === "in-process" ? (
        <p className="text-xs accent">ships with c0mpute · installed by default</p>
      ) : (
        <CodeBlock>{`$ ${installCommand(p)}`}</CodeBlock>
      )}

      <footer className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-[var(--color-dim)]">
        {p.surfaces?.cli && (
          <span>
            cli: <code className="text-[var(--color-fg)]">{p.surfaces.cli}</code>
          </span>
        )}
        {p.homepage && (
          <a href={p.homepage} className="!border-0 hover:text-[var(--color-accent)]">
            homepage ↗
          </a>
        )}
        {p.source && (
          <a href={p.source} className="!border-0 hover:text-[var(--color-accent)]">
            source ↗
          </a>
        )}
        {p.license && <span>license: {p.license}</span>}
      </footer>
    </article>
  );
}
