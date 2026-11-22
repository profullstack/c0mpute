import Link from "next/link";
import { GALLERY, GALLERY_CATEGORIES, SUBMIT_URL, type GalleryCategory } from "@/lib/gallery";

export const metadata = {
  title: "gallery: built on c0mpute — c0mpute",
  description:
    "Apps, automations, music, podcasts and fine-tuned models built with or hosted on the c0mpute network. Every entry is live; most link to both the site and its copy on c0mpute hosting.",
  alternates: { canonical: "https://c0mpute.com/gallery" },
};

const LABEL = Object.fromEntries(GALLERY_CATEGORIES.map((c) => [c.id, c.label])) as Record<GalleryCategory, string>;
const host = (u: string) => new URL(u).host + (new URL(u).pathname === "/" ? "" : new URL(u).pathname.replace(/\/$/, ""));

// Filtering is a plain link with ?category=, so it works with JS off and the
// filtered view has its own URL to share.
export default async function GalleryPage({ searchParams }: { searchParams: Promise<{ category?: string }> }) {
  const { category } = await searchParams;
  const active = GALLERY_CATEGORIES.some((c) => c.id === category) ? (category as GalleryCategory) : null;
  const entries = active ? GALLERY.filter((e) => e.category === active) : GALLERY;

  const pill = (on: boolean) =>
    `!border px-3 py-1 rounded-full whitespace-nowrap ${
      on
        ? "!border-[var(--color-accent)] text-[var(--color-accent)] bg-[var(--color-accent-dim)]/30"
        : "!border-[var(--color-rule)] text-[var(--color-dim)] hover:text-[var(--color-accent)] hover:!border-[var(--color-accent)]"
    }`;

  return (
    <div className="max-w-6xl mx-auto px-4 sm:px-6 py-12 sm:py-16 space-y-10">
      <header className="space-y-3 max-w-3xl">
        <h1 className="text-2xl font-bold accent">gallery: built on c0mpute</h1>
        <p className="comment">// live things that run on the network: models it served, songs and voices it rendered, sites it hosts</p>
        <p className="text-sm leading-7">
          Each card links to the live site and, when it is hosted on c0mpute, to its copy on a gateway. The same list is
          public JSON at <a href="/api/v1/gallery">/api/v1/gallery</a>.
        </p>
      </header>

      <nav aria-label="Filter by category" className="flex flex-wrap gap-2 text-sm">
        <Link href="/gallery" className={pill(!active)} aria-current={!active ? "page" : undefined} scroll={false}>
          all <span className="opacity-60">{GALLERY.length}</span>
        </Link>
        {GALLERY_CATEGORIES.map((c) => {
          const n = GALLERY.filter((e) => e.category === c.id).length;
          if (!n) return null;
          return (
            <Link
              key={c.id}
              href={`/gallery?category=${c.id}`}
              className={pill(active === c.id)}
              aria-current={active === c.id ? "page" : undefined}
              scroll={false}
            >
              {c.label} <span className="opacity-60">{n}</span>
            </Link>
          );
        })}
      </nav>

      <ul className="gallery-grid subgrid-cards" style={{ ["--card-rows" as string]: 4 }}>
        {entries.map((e) => (
          <li key={e.slug} id={e.slug} className="gallery-card subgrid-card squircle">
            <a href={e.url} className="gallery-shot !border-0" tabIndex={-1} aria-hidden="true">
              {/* eslint-disable-next-line @next/next/no-img-element */}
              <img src={e.image} alt="" width={1280} height={800} loading="lazy" decoding="async" />
            </a>
            <div className="space-y-1">
              <h2 className="font-bold text-[var(--color-fg)] text-base">
                <a href={e.url} className="!border-0 text-[var(--color-fg)] hover:text-[var(--color-accent)]">
                  {e.name}
                </a>
              </h2>
              <p className="text-xs text-[var(--color-dim)]">
                {LABEL[e.category]} · live since {e.added_at}
              </p>
            </div>
            <p className="text-sm leading-6">{e.tagline}</p>
            <div className="space-y-3">
              <ul className="flex flex-wrap gap-1.5 text-[11px]" aria-label="built with">
                {e.built_with.map((b) => (
                  <li key={b} className="px-2 py-0.5 rounded border border-[var(--color-rule)] text-[var(--color-dim)]">
                    {b}
                  </li>
                ))}
              </ul>
              <p className="flex flex-col gap-1 text-xs">
                <a href={e.url}>
                  <span aria-hidden="true">→ </span>
                  {host(e.url)}
                </a>
                {e.c0mpute_url && (
                  <a href={e.c0mpute_url}>
                    <span aria-hidden="true">→ </span>c0mpute copy<span className="sr-only"> of {e.name}</span>
                  </a>
                )}
              </p>
            </div>
          </li>
        ))}
      </ul>

      <section className="rule pt-8 space-y-3 text-sm leading-7 max-w-3xl" aria-labelledby="submit">
        <h2 id="submit" className="font-semibold text-[var(--color-fg)]">[ built something on c0mpute? ]</h2>
        <p>
          Host a site with <code>c0mpute hosting publish</code>, call models from the{" "}
          <Link href="/docs">API</Link>, or <Link href="/train">fine-tune an agent</Link>, then tell us about it. We add
          entries that are live and use the network for real.
        </p>
        <p className="flex flex-wrap gap-3">
          <a href={SUBMIT_URL} className="!border px-4 py-2 rounded !border-[var(--color-accent)] hover:bg-[var(--color-accent-dim)]/30">
            submit your build
          </a>
          <a href="mailto:hello@c0mpute.com?subject=Gallery%20submission" className="px-1 py-2">
            or email hello@c0mpute.com
          </a>
        </p>
      </section>
    </div>
  );
}
