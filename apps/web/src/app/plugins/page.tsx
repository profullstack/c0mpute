import Link from "next/link";

import { CodeBlock } from "@/components/CodeBlock";
import {
  loadAllPlugins,
  byCategory,
  tagline,
  installCommand,
  CATEGORY_LABELS,
  CATEGORY_BLURBS,
  type PluginManifest,
} from "@/lib/plugins";

export const metadata = {
  title: "plugins — c0mpute",
  description:
    "c0mpute workload plugins, network services and applications: AI inference, embeddings, diffusion, speech and OCR; transcoding, rendering and streaming; crawling, storage and hosting. MIT-licensed, manifest-driven.",
  alternates: { canonical: "https://c0mpute.com/plugins" },
};

export default function PluginsPage() {
  const groups = byCategory(loadAllPlugins());

  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">plugins</h1>
        <p className="comment">
          // typed workloads, network services, and applications built on them
        </p>
      </header>

      <section className="space-y-2 text-sm leading-6">
        <p>
          Plugins define what the network can execute. A workload plugin is a
          priceable, verifiable contract — an input schema, an output schema,
          capability requirements, pricing units and validation hooks — not an
          invitation to run arbitrary code on someone&apos;s machine.
        </p>
        <p>
          Third-party plugins install with{" "}
          <code>c0mpute plugin install &lt;url&gt;</code> where{" "}
          <code>&lt;url&gt;</code> points at the plugin&apos;s{" "}
          <code>install.sh</code>. See{" "}
          <a href="https://github.com/profullstack/c0mpute/blob/master/dips/0006-module-model.md">
            DIP-0006
          </a>{" "}
          for the model.
        </p>
      </section>

      {/* The three labels matter: an application built on c0mpute should not
          read as a core protocol dependency just because it ships a
          manifest. */}
      <section className="space-y-2 text-xs leading-6 border border-[var(--color-rule)] rounded-md px-4 py-3">
        <p className="text-[var(--color-dim)]">// how to read the labels</p>
        <p>
          <Badge label="workload" /> executes jobs the network prices and
          verifies.{" "}
          <Badge label="service" /> provides a network capability other
          workloads use.{" "}
          <Badge label="application" /> is a product built <em>on</em>{" "}
          c0mpute — useful, and not something the protocol depends on.
        </p>
      </section>

      {groups.map(({ category, plugins }) => (
        <section key={category} className="space-y-4">
          <header className="space-y-1 rule pt-6">
            <h2 className="text-sm font-semibold accent">
              [ {CATEGORY_LABELS[category]} ]
              <span className="text-[var(--color-dim)] font-normal">
                {" "}
                · {plugins.length}
              </span>
            </h2>
            <p className="text-xs text-[var(--color-dim)]">
              {CATEGORY_BLURBS[category]}
            </p>
          </header>
          {plugins.map((p) => (
            <PluginCard key={p.id} p={p} />
          ))}
        </section>
      ))}

      <section className="rule pt-8 text-sm text-[var(--color-dim)] space-y-2">
        <p>
          Want to publish a plugin? Open a PR adding{" "}
          <code>plugins/&lt;your-id&gt;/module.toml</code> on{" "}
          <a href="https://github.com/profullstack/c0mpute">
            github.com/profullstack/c0mpute
          </a>
          , including a <code>category</code>. This page renders from those
          manifests at build time.
        </p>
        <p className="text-xs">
          A submission API lands once plugin signing is in place — signed
          manifests, publisher keys and content hashes. Until then a remote{" "}
          <code>install.sh</code> is the trust boundary, which is exactly why
          it is not the long-term answer.
        </p>
      </section>

      <p className="text-xs text-[var(--color-dim)]">
        → <Link href="/protocol">protocol</Link> ·{" "}
        <Link href="/docs">docs</Link> ·{" "}
        <Link href="/getting-started">getting-started</Link>
      </p>
    </div>
  );
}

/** workload | service | application — see the legend above. */
function kindLabel(p: PluginManifest): string {
  if (p.kind === "workload") return "workload";
  return p.category === "applications" ? "application" : "service";
}

function Badge({ label }: { label: string }) {
  return (
    <span className="border border-[var(--color-rule)] rounded px-1.5 py-0.5 accent whitespace-nowrap">
      {label}
    </span>
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
        <h3 className="text-lg accent">
          <span className="text-[var(--color-dim)]">[</span>
          {p.id}
          <span className="text-[var(--color-dim)]">]</span>{" "}
          <span className="text-[var(--color-fg)]">{p.name}</span>
        </h3>
        <span className="text-xs text-[var(--color-dim)] whitespace-nowrap">
          v{p.version} · {kindLabel(p)} · {dispatchLabel}
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
