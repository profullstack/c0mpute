import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { Markdown, SourceCards } from "@/app/chat/markdown";
import { shareStore, siteUrl } from "@/lib/chat/server";
import type { Share } from "@/lib/chat/share-store";
import { advisorLink } from "@/lib/advisor";
import { splitCommand } from "@/lib/chat/command-registry";

// A shared chat, read by anyone with the link: no sign-in, no app. Written for
// the person Anthony shares with (his mom is 81): big type, plain labels
// ("Question" / "Answer"), the photos and links up front, one obvious button.

export const dynamic = "force-dynamic";

async function load(id: string): Promise<Share | null> {
  return shareStore()
    .get(id)
    .catch(() => null);
}

/** The first answer as one plain line (markdown, citations and links stripped). */
function plainLine(s: string, max = 180): string {
  const t = s
    .replace(/```[\s\S]*?```/g, " ")
    .replace(/\[(\d{1,2})\]/g, "")
    .replace(/\[([^\]]+)\]\([^)]+\)/g, "$1")
    .replace(/^[#>\-*|\s]+/gm, "")
    .replace(/[*_`|]/g, "")
    .replace(/\s+/g, " ")
    .trim();
  return t.length > max ? `${t.slice(0, max - 1).replace(/\s+\S*$/, "")}…` : t;
}

export async function generateMetadata({ params }: { params: Promise<{ id: string }> }): Promise<Metadata> {
  const share = await load((await params).id);
  if (!share) return { title: "Shared chat — c0mpute", robots: { index: false } };
  const answer = share.messages.find((m) => m.role === "assistant")?.content ?? "";
  const description = plainLine(answer) || "A conversation shared from c0mpute chat.";
  const url = `${siteUrl()}/share/${share.id}`;
  return {
    title: `${share.title} — shared chat`,
    description,
    robots: { index: false, follow: false },
    alternates: { canonical: url },
    openGraph: { type: "article", siteName: "c0mpute", title: share.title, description, url },
    twitter: { card: "summary_large_image", title: share.title, description },
  };
}

export default async function SharePage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  const share = await load(id);
  if (!share) notFound();
  void shareStore()
    .view(id)
    .catch(() => {});
  // Agentic Advisor: Finance answers carry where they were made (with a referral handle).
  const advisor = share.messages.some((m) => m.role === "user" && splitCommand(m.content)?.command.pack === "finance");
  const date = new Date(share.updated).toLocaleDateString("en-US", { year: "numeric", month: "long", day: "numeric" });
  return (
    <main className="max-w-2xl mx-auto px-4 sm:px-6 py-8 text-[19px] leading-[1.65]">
      <p className="text-[15px] text-[var(--color-dim)]">Someone shared this conversation with you · {date}</p>
      <h1 className="mt-2 mb-6 text-[26px] leading-tight font-bold text-[var(--color-fg)]">{share.title}</h1>

      <div className="space-y-8">
        {share.messages.map((m, i) =>
          m.role === "user" ? (
            <section key={i} aria-label="Question" className="border-l-4 border-[var(--color-accent,currentColor)] pl-4">
              <div className="text-[15px] font-bold uppercase tracking-wide text-[var(--color-dim)]">Question</div>
              <p className="mt-1 whitespace-pre-wrap break-words text-[var(--color-fg)]">{m.content}</p>
            </section>
          ) : (
            <section key={i} aria-label="Answer">
              <div className="text-[15px] font-bold uppercase tracking-wide text-[var(--color-dim)]">Answer</div>
              {m.sources?.length ? <SourceCards sources={m.sources} /> : null}
              <div className="mt-1 text-[var(--color-fg)]">
                <Markdown text={m.content} cites={m.sources} />
              </div>
            </section>
          ),
        )}
      </div>

      <p className="mt-10 text-[15px] text-[var(--color-dim)]">
        Answers are written by an AI and can be wrong. Numbers in brackets like [1] link to the web pages the answer used, so you can check them.
      </p>

      {advisor ? (
        <p className="mt-4 text-[15px] text-[var(--color-dim)]">
          Analyzed on c0mpute: <a href={advisorLink()} className="underline break-all">{advisorLink()}</a> · Analysis, not financial, tax or legal advice.
        </p>
      ) : null}

      <div className="mt-6 flex flex-col sm:flex-row gap-3">
        <Link
          href={`/chat?import=${encodeURIComponent(share.id)}`}
          className="inline-block text-center px-5 py-3 text-[18px] font-bold border-2 border-[var(--color-fg)] text-[var(--color-fg)] no-underline hover:bg-[var(--color-fg)] hover:text-[var(--color-bg)]"
        >
          Continue this conversation
        </Link>
        <Link href="/chat" className="inline-block text-center px-5 py-3 text-[18px] border border-[var(--color-rule)] text-[var(--color-fg)] no-underline hover:border-[var(--color-fg)]">
          Ask your own question
        </Link>
      </div>
    </main>
  );
}
