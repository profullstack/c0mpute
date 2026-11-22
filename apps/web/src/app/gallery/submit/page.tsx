import Link from "next/link";
import { BUILT_WITH, GALLERY_CATEGORIES } from "@/lib/gallery";
import { BADGE_ALT, BADGE_HEIGHT, BADGE_WIDTH, badgeSnippet } from "@/lib/gallery/badge";
import { GALLERY_CONFIG } from "@/lib/gallery/config";
import { SubmitForm } from "./submit-form";

export const metadata = {
  title: "get listed on the c0mpute gallery — c0mpute",
  description: `Add what you built on c0mpute to the gallery. Free: place our badge and go live as soon as we see it, or wait ${GALLERY_CONFIG.freeQueueDays} days. Priority: $${GALLERY_CONFIG.priorityPriceUsd} once, live as soon as the automated checks pass.`,
  alternates: { canonical: "https://c0mpute.com/gallery/submit" },
};

export default function SubmitPage() {
  const price = GALLERY_CONFIG.priorityPriceUsd;
  return (
    <div className="max-w-4xl mx-auto px-4 sm:px-6 py-12 sm:py-16 space-y-10">
      <header className="space-y-3 max-w-3xl">
        <p className="text-sm">
          <Link href="/gallery">← gallery</Link>
        </p>
        <h1 className="text-2xl font-bold accent">get listed</h1>
        <p className="comment">// built something with c0mpute? put it in the gallery, on c0mpute.com and infernetprotocol.com</p>
      </header>

      <section aria-label="tiers" className="grid gap-4 sm:grid-cols-2">
        <div className="rounded-lg border border-[var(--color-rule)] bg-[var(--color-card)] p-5 space-y-3">
          <h2 className="font-bold text-[var(--color-fg)]">free</h2>
          <p className="text-2xl font-bold">$0</p>
          <ul className="text-sm leading-7 list-none space-y-1">
            <li>→ put our badge on your site: listed as soon as we see it</li>
            <li>→ no badge: listed after a {GALLERY_CONFIG.freeQueueDays}-day queue</li>
            <li>→ we re-check the badge every {GALLERY_CONFIG.badgeRecheckDays} days; if it goes, you go back to the queue</li>
          </ul>
        </div>
        <div className="rounded-lg border border-[var(--color-accent-dim)] bg-[var(--color-card)] p-5 space-y-3">
          <h2 className="font-bold text-[var(--color-fg)]">priority</h2>
          <p className="text-2xl font-bold">
            ${price} <span className="text-sm font-normal text-[var(--color-dim)]">once, crypto or PayPal via CoinPay</span>
          </p>
          <ul className="text-sm leading-7 list-none space-y-1">
            <li>→ live as soon as the payment confirms and the automated checks pass (site answers, not blocklisted, not adult)</li>
            <li>→ no badge needed, no re-checks</li>
            <li>→ not listed after all? refunded</li>
          </ul>
        </div>
      </section>

      <section aria-labelledby="badge" className="space-y-3 text-sm leading-7">
        <h2 id="badge" className="font-semibold text-[var(--color-fg)]">[ the badge ]</h2>
        <p>
          Keyed by your site&apos;s URL only: no account, no product id. Paste it anywhere on the page you list, or on your
          homepage (a footer is fine). <code>&amp;theme=dark</code> for dark sites. It loads no script and sets no cookie.
        </p>
        <p className="flex flex-wrap items-center gap-4">
          {/* eslint-disable-next-line @next/next/no-img-element */}
          <img src="/badge.svg" alt={BADGE_ALT} width={BADGE_WIDTH} height={BADGE_HEIGHT} />
          {/* eslint-disable-next-line @next/next/no-img-element */}
          <img src="/badge.svg?theme=dark" alt={BADGE_ALT} width={BADGE_WIDTH} height={BADGE_HEIGHT} />
        </p>
        <pre className="whitespace-pre-wrap break-all text-xs p-3 rounded border border-[var(--color-rule)] bg-[var(--color-panel)]">
          {badgeSnippet("https://example.com")}
        </pre>
        <p className="text-xs text-[var(--color-dim)]">
          Profullstack sites get it from <code>@profullstack/footer</code> with the c0mpute badge option on.
        </p>
      </section>

      <SubmitForm categories={GALLERY_CATEGORIES.map((c) => ({ id: c.id, label: c.label }))} builtWith={[...BUILT_WITH]} price={price} queueDays={GALLERY_CONFIG.freeQueueDays} />

      <section className="rule pt-6 text-sm leading-7 space-y-2">
        <h2 className="font-semibold text-[var(--color-fg)]">[ from a terminal or an agent ]</h2>
        <pre className="whitespace-pre-wrap break-all text-xs p-3 rounded border border-[var(--color-rule)] bg-[var(--color-panel)]">{`c0mpute gallery submit https://example.com --name "Example" --email you@example.com
c0mpute gallery status https://example.com
curl -X POST https://c0mpute.com/api/v1/gallery/submissions -H 'content-type: application/json' \\
  -d '{"url":"https://example.com","tier":"free","contact_email":"you@example.com"}'`}</pre>
        <p>
          MCP: <code>gallery_submit</code>, <code>gallery_status</code> and <code>gallery_list</code> on{" "}
          <code>https://c0mpute.com/api/mcp</code>. API reference: <a href="https://github.com/profullstack/c0mpute/blob/master/docs/gallery-api.md">docs/gallery-api.md</a>.
        </p>
      </section>
    </div>
  );
}
