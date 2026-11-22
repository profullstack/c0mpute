import Link from "next/link";
import { notFound } from "next/navigation";
import { expertBySlug, experts } from "@/lib/experts/catalog";
import { expertPriceUsd, expertsConfig } from "@/lib/experts/config";
import { ExpertPanel } from "./expert-panel";

export const dynamic = "force-dynamic";

export async function generateMetadata({ params }: { params: Promise<{ slug: string }> }) {
  const { slug } = await params;
  const m = expertBySlug(slug);
  if (!m) return { title: "expert not found — c0mpute" };
  return {
    title: `${m.name}: c0mpute expert — c0mpute`,
    description: m.summary,
    alternates: { canonical: `https://c0mpute.com/experts/${m.slug}` },
  };
}

export default async function ExpertPage({ params }: { params: Promise<{ slug: string }> }) {
  const { slug } = await params;
  const m = expertBySlug(slug);
  if (!m) notFound();
  const cfg = expertsConfig();
  const price = expertPriceUsd(m.pricing.monthly_usd, cfg);
  const others = experts().filter((x) => x.slug !== m.slug);
  return (
    <div className="max-w-4xl mx-auto px-4 sm:px-6 py-12 sm:py-16 space-y-8">
      <header className="space-y-3">
        <p className="text-xs text-[var(--color-dim)]">
          <Link href="/experts">experts</Link> / {m.slug}
        </p>
        <h1 className="text-2xl font-bold accent">{m.name}</h1>
        <p className="comment">// {m.summary}</p>
        <p className="text-sm leading-7">{m.description}</p>
        <ul className="flex flex-wrap gap-1.5 text-[11px]" aria-label="skills">
          {m.skills.map((s) => (
            <li key={s} className="px-2 py-0.5 rounded border border-[var(--color-rule)] text-[var(--color-dim)]">
              {s}
            </li>
          ))}
        </ul>
      </header>

      {m.disclaimer && (
        <aside role="note" className="border border-[var(--color-warn)] rounded p-3 text-sm leading-6" style={{ color: "var(--color-warn)" }}>
          <strong>Information, not advice.</strong> {m.disclaimer}
        </aside>
      )}

      <ExpertPanel
        slug={m.slug}
        name={m.name}
        disclaimer={m.disclaimer ?? null}
        examples={m.example_questions}
        price={price}
        bundlePrice={cfg.bundlePriceUsd}
        periodDays={cfg.periodDays}
        trialDays={cfg.trialDays}
      />

      <section className="text-sm leading-7 space-y-2">
        <h2 className="font-bold">details</h2>
        <ul className="list-disc pl-5 space-y-1">
          <li>
            Publisher: {m.publisher.name} ({m.publisher.did ?? m.publisher.id}), manifest v{m.version}, signed{" "}
            <code className="break-all text-xs">{m.signature?.key}</code>
          </li>
          <li>Models tried in order: {m.model.prefer.map((x) => <code key={x} className="mr-1">{x}</code>)}</li>
          <li>
            MCP plugins it may use when you connect them in <Link href="/chat">chat</Link>:{" "}
            {m.tools.mcp.length ? m.tools.mcp.map((t) => <code key={t.id} className="mr-1" title={t.why}>{t.id}</code>) : "none"}
          </li>
          <li>
            API: <code>POST /v1/chat/completions</code> with <code>{`{"model": "expert:${m.slug}", "messages": [...]}`}</code> · manifest{" "}
            <a href={`/api/v1/experts/${m.slug}`}>/api/v1/experts/{m.slug}</a>
          </li>
          <li>
            CLI: <code>c0mpute ask {m.slug} &quot;your question&quot;</code> · MCP: <code>ask_expert</code>
          </li>
          <li>
            Price: free for {cfg.trialDays} days (one trial per CoinPay account), then ${price} per {cfg.periodDays} days, or ${cfg.bundlePriceUsd} for every expert.
          </li>
        </ul>
      </section>

      <nav aria-label="Other experts" className="text-sm flex flex-wrap gap-3">
        {others.map((o) => (
          <Link key={o.slug} href={`/experts/${o.slug}`}>
            <span aria-hidden="true">→ </span>
            {o.name}
          </Link>
        ))}
      </nav>
    </div>
  );
}
