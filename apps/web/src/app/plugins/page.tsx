import Link from "next/link";

import { CodeBlock } from "@/components/CodeBlock";
import {
  loadAllPlugins,
  byCategory,
  tagline,
  installCommand,
  installedByDefault,
  modelOptions,
  CATEGORY_LABELS,
  CATEGORY_BLURBS,
  type PluginManifest,
  type ModelOption,
} from "@/lib/plugins";

export const metadata = {
  title: "plugins — c0mpute",
  description:
    "c0mpute workload plugins, network services and applications: AI inference, image and video generation, speech, transcoding, web fetching, workflow steps, webhooks and OpenDisk storage, with more marked coming soon. MIT-licensed, manifest-driven.",
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
        <p>
          <Badge label="partial" /> runs today with a caveat noted on the
          card.{" "}
          <Badge label="coming soon" /> is a published spec with no
          implementation yet — nothing to install or run.
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
                {comingSoonCount(plugins) > 0 &&
                  ` (${comingSoonCount(plugins)} coming soon)`}
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

function comingSoonCount(plugins: PluginManifest[]): number {
  return plugins.filter((p) => p.status === "coming-soon").length;
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
  const comingSoon = p.status === "coming-soon";
  const install = installCommand(p);

  return (
    <article
      className={`border border-[var(--color-rule)] bg-[var(--color-card)] rounded p-5 space-y-3${comingSoon ? " opacity-70" : ""}`}
    >
      <header className="flex items-baseline justify-between gap-3">
        <h3 className="text-lg accent">
          <span className="text-[var(--color-dim)]">[</span>
          {p.id}
          <span className="text-[var(--color-dim)]">]</span>{" "}
          <span className="text-[var(--color-fg)]">{p.name}</span>
          {p.status !== "available" && (
            <>
              {" "}
              <span className="text-xs align-middle">
                <Badge label={comingSoon ? "coming soon" : "partial"} />
              </span>
            </>
          )}
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

      <ModelList p={p} />

      {typeof p.requirements?.hardware === "string" && (
        <p className="text-xs text-[var(--color-dim)]">
          <span className="accent">requires:</span> {p.requirements.hardware}
        </p>
      )}

      {p.status === "partial" && p.statusNote && (
        <p className="text-xs text-[var(--color-dim)]">
          <span className="accent">partial:</span> {p.statusNote}
        </p>
      )}

      {comingSoon ? (
        <p className="text-xs text-[var(--color-dim)]">
          coming soon · spec only, not yet implemented
        </p>
      ) : installedByDefault(p) ? (
        <p className="text-xs accent">ships with c0mpute · installed by default</p>
      ) : install ? (
        <CodeBlock>{`$ ${install}`}</CodeBlock>
      ) : null}

      <footer className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-[var(--color-dim)]">
        {p.surfaces?.cli && !comingSoon && (
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

/**
 * Model options declared by the manifest. Rendered as a plain list rather
 * than a table: the point a reader is after is "which of these can my card
 * run", so the VRAM floor sits next to each name.
 */
function ModelList({ p }: { p: PluginManifest }) {
  const models = modelOptions(p);
  if (models.length === 0) return null;

  return (
    <div className="space-y-1">
      <p className="text-xs accent">models</p>
      <ul className="space-y-1 text-xs text-[var(--color-dim)]">
        {models.map(({ id, model }) => (
          <li key={id}>
            <code className="text-[var(--color-fg)]">{id}</code>
            {model.default && <span className="accent"> · default</span>}
            {modelSpec(model).length > 0 && <> · {modelSpec(model).join(" · ")}</>}
            {model.repo && (
              <>
                {" · "}
                <a
                  href={`https://huggingface.co/${model.repo}`}
                  className="!border-0 hover:text-[var(--color-accent)]"
                >
                  {model.repo} ↗
                </a>
              </>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}

/** The specs worth showing inline, skipping whatever the manifest omitted. */
function modelSpec(model: ModelOption): string[] {
  return [
    model.params,
    model.precision,
    model.vram_gib ? `${model.vram_gib} GiB VRAM` : undefined,
    model.resolution,
    model.audio ? "video + audio" : undefined,
  ].filter((v): v is string => typeof v === "string" && v.length > 0);
}
