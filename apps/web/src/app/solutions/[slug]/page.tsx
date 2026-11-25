import Link from "next/link";
import { notFound } from "next/navigation";
import { CodeBlock } from "@/components/CodeBlock";
import type { GalleryEntry } from "@/lib/gallery";
import { galleryEntries } from "@/lib/gallery/server";
import { SOLUTIONS, solutionBySlug, solutionPath, type SolutionLink } from "@/lib/solutions";

// "Built with this" follows the gallery, which changes without a deploy.
export const dynamic = "force-dynamic";

type Params = { params: Promise<{ slug: string }> };

export async function generateMetadata({ params }: Params) {
  const { slug } = await params;
  const s = solutionBySlug(slug);
  if (!s) return { title: "solution not found — c0mpute" };
  return {
    title: `${s.label}: install, example and use cases — c0mpute`,
    description: `${s.summary} Install, a working example and ${s.useCases.length} use cases with examples.`,
    alternates: { canonical: `https://c0mpute.com${solutionPath(s)}` },
  };
}

function Go({ link }: { link: SolutionLink }) {
  return link.href.startsWith("/") ? (
    <Link href={link.href}>{link.label}</Link>
  ) : (
    <a href={link.href} target="_blank" rel="noopener noreferrer">
      {link.label}
    </a>
  );
}

export default async function SolutionPage({ params }: Params) {
  const { slug } = await params;
  const s = solutionBySlug(slug);
  if (!s) notFound();

  const { entries } = await galleryEntries().catch(() => ({ entries: [] as GalleryEntry[] }));
  const built = entries.filter((e) => e.built_with.includes(s.label));
  const others = SOLUTIONS.filter((o) => o.slug !== s.slug);

  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
      <header className="space-y-2">
        <p className="text-xs text-[var(--color-dim)]">
          <Link href="/solutions">solutions</Link> /
        </p>
        <h1 className="text-2xl font-bold accent">{s.label}</h1>
        <p className="comment">// {s.desc}</p>
        <p className="text-sm leading-7 pt-2">{s.summary}</p>
      </header>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ 1. install ]</h2>
        <CodeBlock>{s.install}</CodeBlock>
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ 2. example ]</h2>
        <CodeBlock>{s.example}</CodeBlock>
        <p className="text-[var(--color-dim)]">{s.result}</p>
      </section>

      <section className="space-y-6 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ use cases ]</h2>
        {s.useCases.map((u) => (
          <article key={u.title} className="space-y-2">
            <h3 className="accent">{u.title}</h3>
            <p>{u.text}</p>
            <CodeBlock>{u.example}</CodeBlock>
          </article>
        ))}
      </section>

      <section className="space-y-3 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ built with {s.label} ]</h2>
        {built.length > 0 ? (
          <ul className="space-y-3">
            {built.map((e) => (
              <li key={e.slug} className="border border-[var(--color-rule)] bg-[var(--color-card)] rounded p-4 space-y-1">
                <p>
                  <a href={e.url} target="_blank" rel="noopener noreferrer" className="accent">
                    {e.name}
                  </a>
                </p>
                <p className="text-xs text-[var(--color-dim)] leading-6">{e.tagline}</p>
                <p className="text-xs">
                  <Link href={`/gallery#${e.slug}`}>in the gallery</Link>
                  {e.c0mpute_url && (
                    <>
                      {" · "}
                      <a href={e.c0mpute_url} target="_blank" rel="noopener noreferrer">
                        its copy on c0mpute
                      </a>
                    </>
                  )}
                </p>
              </li>
            ))}
          </ul>
        ) : (
          <p className="text-[var(--color-dim)]">
            Nothing in the <Link href="/gallery">gallery</Link> lists this yet.
          </p>
        )}
        <p>
          Built something with it? <Link href="/gallery/submit">Get it listed</Link>.
        </p>
      </section>

      <section className="space-y-2 rule pt-8 text-sm leading-7">
        <h2 className="font-semibold text-[var(--color-fg)]">[ next ]</h2>
        <ul className="space-y-1 text-xs">
          {s.links.map((l) => (
            <li key={l.href}>
              → <Go link={l} />
            </li>
          ))}
        </ul>
        <p className="text-xs text-[var(--color-dim)] pt-3 leading-6">
          other solutions:{" "}
          {others.map((o, i) => (
            <span key={o.slug}>
              {i > 0 && " · "}
              <Link href={solutionPath(o)}>{o.label}</Link>
            </span>
          ))}
        </p>
      </section>
    </div>
  );
}
