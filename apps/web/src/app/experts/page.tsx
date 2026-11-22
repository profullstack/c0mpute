import Link from "next/link";
import { experts } from "@/lib/experts/catalog";
import { expertPriceUsd, expertsConfig } from "@/lib/experts/config";

export const dynamic = "force-dynamic";

export const metadata = {
  title: "experts: security, sales, automation, legal and tax — c0mpute",
  description:
    "First-party AI experts on the c0mpute network: a security analyst, a sales coach, an automation engineer, and legal and tax information. 90-day free trial with CoinPay, then a monthly subscription.",
  alternates: { canonical: "https://c0mpute.com/experts" },
};

const CATEGORY_LABEL: Record<string, string> = { security: "security", sales: "sales", automation: "automation", legal: "legal info", tax: "tax info" };

export default function ExpertsPage() {
  const cfg = expertsConfig();
  const list = experts();
  return (
    <div className="max-w-6xl mx-auto px-4 sm:px-6 py-12 sm:py-16 space-y-10">
      <header className="space-y-3 max-w-3xl">
        <h1 className="text-2xl font-bold accent">experts</h1>
        <p className="comment">// first-party AI experts, answered by open models on the c0mpute network</p>
        <p className="text-sm leading-7">
          Each expert is a signed manifest: a model choice, a system prompt that keeps it in scope, and the MCP plugins it may use. Ask from this site,
          the API (<code>model: &quot;expert:&lt;slug&gt;&quot;</code> on <code>/v1/chat/completions</code>), MCP (<code>ask_expert</code>) or the CLI (
          <code>c0mpute ask security-analyst &quot;...&quot;</code>).
        </p>
        <p className="text-sm leading-7">
          <strong className="accent">{cfg.trialDays} days free</strong>, one trial per CoinPay account, covering every expert. Then ${expertPriceUsd(9, cfg)} per expert or ${cfg.bundlePriceUsd} for all of them,
          per {cfg.periodDays} days, paid with CoinPay.
        </p>
      </header>

      <ul className="gallery-grid subgrid-cards" style={{ ["--card-rows" as string]: 4 }}>
        {list.map((m) => (
          <li key={m.slug} id={m.slug} className="gallery-card subgrid-card squircle">
            <div className="space-y-1">
              <h2 className="font-bold text-base">
                <Link href={`/experts/${m.slug}`} className="!border-0 text-[var(--color-fg)] hover:text-[var(--color-accent)]">
                  {m.name}
                </Link>
              </h2>
              <p className="text-xs text-[var(--color-dim)]">
                {CATEGORY_LABEL[m.category] ?? m.category} · by {m.publisher.name} · v{m.version}
              </p>
            </div>
            <p className="text-sm leading-6">{m.summary}</p>
            <ul className="flex flex-wrap gap-1.5 text-[11px]" aria-label="skills">
              {m.skills.slice(0, 5).map((s) => (
                <li key={s} className="px-2 py-0.5 rounded border border-[var(--color-rule)] text-[var(--color-dim)]">
                  {s}
                </li>
              ))}
            </ul>
            <p className="flex flex-wrap items-center justify-between gap-2 text-xs">
              <span className="text-[var(--color-dim)]">
                ${expertPriceUsd(m.pricing.monthly_usd, cfg)}/{cfg.periodDays}d after the free trial
                {m.disclaimer ? " · information, not advice" : ""}
              </span>
              <Link href={`/experts/${m.slug}`}>
                <span aria-hidden="true">→ </span>ask {m.name.toLowerCase()}
              </Link>
            </p>
          </li>
        ))}
      </ul>

      <section className="text-sm leading-7 max-w-3xl space-y-2">
        <h2 className="font-bold">how it works</h2>
        <ul className="list-disc pl-5 space-y-1">
          <li>Sign in with CoinPay. Your first question starts the free trial; your account page shows when it ends.</li>
          <li>Answers come from open-weight models on peer nodes, billed like any c0mpute chat (your compute balance, or the free daily allowance).</li>
          <li>Experts stay in their lane, say when they are unsure, and never invent citations. Legal and tax experts give general information, not advice.</li>
          <li>
            Manifests are public and signed: <a href="/api/v1/experts">/api/v1/experts</a>. The format is the one the open c0mpute app store will use (
            <a href="https://github.com/profullstack/c0mpute/blob/master/docs/experts.md">docs/experts.md</a>).
          </li>
        </ul>
      </section>
    </div>
  );
}
