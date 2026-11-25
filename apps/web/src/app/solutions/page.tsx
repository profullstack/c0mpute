import Link from "next/link";
import type { GalleryEntry } from "@/lib/gallery";
import { galleryEntries } from "@/lib/gallery/server";
import { SOLUTIONS, solutionPath } from "@/lib/solutions";

// The "built with" counts follow the gallery, which changes without a deploy.
export const dynamic = "force-dynamic";

export const metadata = {
  title: "solutions: what you can build with c0mpute — c0mpute",
  description:
    "Every c0mpute solution on its own page with a simple install, a working example and use cases: hosting, the OpenAI-compatible chat API, Infernet chat, jobs, music, speech, images, video, fine-tuning, the model router and apps.",
  alternates: { canonical: "https://c0mpute.com/solutions" },
};

export default async function SolutionsPage() {
  const { entries } = await galleryEntries().catch(() => ({ entries: [] as GalleryEntry[] }));
  const count = (label: string) => entries.filter((e) => e.built_with.includes(label)).length;

  return (
    <div className="max-w-6xl mx-auto px-4 sm:px-6 py-12 sm:py-16 space-y-10">
      <header className="space-y-3 max-w-3xl">
        <h1 className="text-2xl font-bold accent">solutions</h1>
        <p className="comment">// what you can build with c0mpute: one page each, with the install, an example and use cases</p>
        <p className="text-sm leading-7">
          Each of these runs on the network of everyday computers behind c0mpute. Pick one to see how to install it,
          the smallest example that works, and what people use it for. The{" "}
          <Link href="/gallery">gallery</Link> shows live things built with each.
        </p>
      </header>

      <ul className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {SOLUTIONS.map((s) => {
          const n = count(s.label);
          return (
            <li key={s.slug}>
              <Link
                href={solutionPath(s)}
                className="!border block h-full rounded-md !border-[var(--color-rule)] bg-[var(--color-card)] p-4 space-y-2 hover:!border-[var(--color-accent)]"
              >
                <h2 className="font-semibold accent">{s.label}</h2>
                <p className="text-xs text-[var(--color-dim)]">{s.desc}</p>
                <p className="text-sm leading-6 text-[var(--color-fg)]">{s.summary}</p>
                <p className="text-xs text-[var(--color-dim)]">
                  {s.useCases.length} use cases{n > 0 ? ` · ${n} in the gallery` : ""}
                </p>
              </Link>
            </li>
          );
        })}
      </ul>

      <p className="text-xs text-[var(--color-dim)] rule pt-6">
        → <Link href="/getting-started">getting started</Link> · <Link href="/docs">docs</Link> ·{" "}
        <Link href="/gallery">gallery</Link> · <Link href="/pricing">pricing</Link>
      </p>
    </div>
  );
}
