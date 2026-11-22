import Link from "next/link";
import { notFound } from "next/navigation";
import { GALLERY_CONFIG } from "@/lib/gallery/config";
import { gallery } from "@/lib/gallery/server";
import { publicStatus } from "@/lib/gallery/service";
import { CheckButton } from "./check-button";

export const dynamic = "force-dynamic";
export const metadata = { title: "gallery listing status — c0mpute", robots: { index: false } };

const LABEL: Record<string, string> = {
  queued: "in the queue",
  awaiting_payment: "waiting for payment",
  priority_review: "priority review",
  review: "waiting for a person",
  approved: "listed",
  rejected: "not listed",
};

const day = (iso: string | null) => (iso ? new Date(iso).toUTCString().slice(0, 16) : "");

export default async function StatusPage({ params, searchParams }: { params: Promise<{ id: string }>; searchParams: Promise<{ paid?: string; new?: string }> }) {
  const { id } = await params;
  const { paid, new: isNew } = await searchParams;
  const l = await gallery().store.byId(id).catch(() => null);
  if (!l) notFound();
  const s = publicStatus(l);
  const canPay = !l.paid_at && (l.status === "queued" || l.status === "awaiting_payment" || l.status === "review");
  const canCheck = l.status === "queued" || l.status === "awaiting_payment" || (l.status === "approved" && l.listed_via === "badge");

  return (
    <div className="max-w-3xl mx-auto px-4 sm:px-6 py-12 sm:py-16 space-y-8 text-sm leading-7">
      <header className="space-y-2">
        <p>
          <Link href="/gallery">← gallery</Link>
        </p>
        <h1 className="text-2xl font-bold accent">{l.name}</h1>
        <p className="text-[var(--color-dim)] break-all">{l.url}</p>
      </header>

      {isNew && <p className="text-[var(--color-accent)]">Received. Keep this page: it is your listing&apos;s status (we emailed the link too if you gave an email).</p>}
      {paid && !l.paid_at && <p className="text-[var(--color-accent)]">Payment received by CoinPay: this page updates once it confirms (usually under a minute). Reload to see it.</p>}

      <section className="rounded-lg border border-[var(--color-rule)] bg-[var(--color-card)] p-5 space-y-2" aria-label="status">
        <p>
          status: <b className="text-[var(--color-fg)]">{LABEL[l.status] ?? l.status}</b>
          {l.paid_at && <span className="text-[var(--color-dim)]"> · paid</span>}
          {l.featured && <span className="accent"> · featured</span>}
        </p>
        {l.status === "queued" && (
          <p>
            goes live on <b className="text-[var(--color-fg)]">{day(l.goes_live_at)}</b>, or as soon as we find the badge on your site.
          </p>
        )}
        {l.status === "approved" && (
          <p>
            live since {l.added_at} · <a href={s.gallery_url!}>see it in the gallery</a>
            {l.listed_via === "badge" && <span className="text-[var(--color-dim)]"> · listed on the badge: keep it on your site (re-checked every {GALLERY_CONFIG.badgeRecheckDays} days)</span>}
          </p>
        )}
        {l.status === "awaiting_payment" && <p>Pay ${GALLERY_CONFIG.priorityPriceUsd} below to finish priority review, or place the badge to list free.</p>}
        {l.status === "priority_review" && <p>Paid. The automated checks held it for a person; we will email you.</p>}
        {l.status === "review" && <p>A person will look at it before it is listed; we will email you.</p>}
        <p className="text-[var(--color-dim)]">
          badge: {l.badge_state ?? "not checked yet"}
          {l.badge_checked_at && ` (checked ${day(l.badge_checked_at)} ${l.badge_checked_at.slice(11, 16)} UTC)`}
          {l.badge_missing_since && ` · missing since ${day(l.badge_missing_since)}`}
        </p>
        {s.note && <p className="text-[var(--color-dim)] break-words">note: {s.note}</p>}
      </section>

      {canCheck && (
        <section className="space-y-3" aria-labelledby="badge-h">
          <h2 id="badge-h" className="font-semibold text-[var(--color-fg)]">[ badge ]</h2>
          <p>Paste this into the page above or your homepage, then check:</p>
          <pre className="whitespace-pre-wrap break-all text-xs p-3 rounded border border-[var(--color-rule)] bg-[var(--color-panel)]">{s.snippet}</pre>
          <CheckButton id={l.id} />
        </section>
      )}

      {canPay && (
        <section className="space-y-3" aria-labelledby="pay-h">
          <h2 id="pay-h" className="font-semibold text-[var(--color-fg)]">[ priority review, ${GALLERY_CONFIG.priorityPriceUsd} ]</h2>
          <p>One payment, any coin or PayPal through CoinPay. Live as soon as it confirms and the automated checks pass.</p>
          <form method="post" action={`/api/v1/gallery/submissions/${l.id}/checkout`}>
            <button type="submit" className="!border px-5 py-2 rounded !border-[var(--color-accent)] text-[var(--color-accent)] hover:bg-[var(--color-accent-dim)]/30">
              pay ${GALLERY_CONFIG.priorityPriceUsd} with CoinPay
            </button>
          </form>
          {!l.contact_email && <p className="text-xs text-[var(--color-warn)]">This listing has no contact email, which CoinPay needs for the receipt: resubmit with one, or email hello@c0mpute.com.</p>}
        </section>
      )}

      <p className="text-xs text-[var(--color-dim)]">
        JSON: <a href={`/api/v1/gallery/submissions/${l.id}`}>/api/v1/gallery/submissions/{l.id}</a>
      </p>
    </div>
  );
}
