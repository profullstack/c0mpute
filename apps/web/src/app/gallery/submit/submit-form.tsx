"use client";

import { useState } from "react";

type Props = { categories: { id: string; label: string }[]; builtWith: string[]; price: number; queueDays: number };
type ApiListing = { id: string; status: string; status_url: string; goes_live_at: string | null };

const field = "w-full px-3 py-2 rounded border border-[var(--color-rule)] bg-[var(--color-panel)] text-[var(--color-fg)] focus:outline-none focus:border-[var(--color-accent)]";

export function SubmitForm({ categories, builtWith, price, queueDays }: Props) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [existing, setExisting] = useState<ApiListing | null>(null);
  const [tier, setTier] = useState<"free" | "priority">("free");

  async function onSubmit(e: React.FormEvent<HTMLFormElement>) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    setExisting(null);
    const f = new FormData(e.currentTarget);
    const body = {
      url: f.get("url"),
      name: f.get("name"),
      tagline: f.get("tagline"),
      category: f.get("category"),
      built_with: f.getAll("built_with"),
      contact_email: f.get("contact_email"),
      tier: f.get("tier"),
    };
    try {
      const res = await fetch("/api/v1/gallery/submissions", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
      const j = (await res.json().catch(() => ({}))) as { listing?: ApiListing; checkout_url?: string | null; error?: { message?: string } };
      if (res.status === 409 && j.listing) {
        setExisting(j.listing);
        setError(j.error?.message ?? "already submitted");
        return;
      }
      if (!res.ok || !j.listing) {
        setError(j.error?.message ?? `could not submit (HTTP ${res.status})`);
        return;
      }
      window.location.assign(j.checkout_url || `/gallery/status/${j.listing.id}?new=1`);
    } catch {
      setError("could not reach c0mpute.com; try again");
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={onSubmit} className="space-y-5 text-sm" aria-labelledby="form-h">
      <h2 id="form-h" className="font-semibold text-[var(--color-fg)]">[ submit ]</h2>
      <label className="block space-y-1">
        <span>site URL <span className="text-[var(--color-dim)]">(required: the page we list and check for the badge)</span></span>
        <input name="url" type="text" inputMode="url" required placeholder="https://example.com" className={field} autoComplete="url" />
      </label>
      <div className="grid gap-4 sm:grid-cols-2">
        <label className="block space-y-1">
          <span>name</span>
          <input name="name" maxLength={60} placeholder="Example" className={field} />
        </label>
        <label className="block space-y-1">
          <span>category</span>
          <select name="category" className={field} defaultValue="apps">
            {categories.map((c) => (
              <option key={c.id} value={c.id}>
                {c.label}
              </option>
            ))}
          </select>
        </label>
      </div>
      <label className="block space-y-1">
        <span>tagline <span className="text-[var(--color-dim)]">(one sentence: what it is and what part runs on c0mpute)</span></span>
        <input name="tagline" maxLength={200} className={field} />
      </label>
      <fieldset className="space-y-2">
        <legend>built with</legend>
        <div className="flex flex-wrap gap-2">
          {builtWith.map((b) => (
            <label key={b} className="inline-flex items-center gap-1.5 px-2 py-1 rounded border border-[var(--color-rule)] cursor-pointer has-[:checked]:border-[var(--color-accent)]">
              <input type="checkbox" name="built_with" value={b} className="accent-[var(--color-accent)]" />
              {b}
            </label>
          ))}
        </div>
      </fieldset>
      <label className="block space-y-1">
        <span>
          contact email <span className="text-[var(--color-dim)]">({tier === "priority" ? "required for the receipt" : "optional; we tell you when you go live"}, never shown)</span>
        </span>
        <input name="contact_email" type="email" required={tier === "priority"} className={field} autoComplete="email" />
      </label>
      <fieldset className="grid gap-3 sm:grid-cols-2">
        <legend className="mb-2">tier</legend>
        <label className="flex gap-2 p-3 rounded border border-[var(--color-rule)] cursor-pointer has-[:checked]:border-[var(--color-accent)]">
          <input type="radio" name="tier" value="free" checked={tier === "free"} onChange={() => setTier("free")} className="accent-[var(--color-accent)]" />
          <span>
            <b className="text-[var(--color-fg)]">free</b>
            <span className="block text-xs text-[var(--color-dim)]">live when we see the badge, or after {queueDays} days</span>
          </span>
        </label>
        <label className="flex gap-2 p-3 rounded border border-[var(--color-rule)] cursor-pointer has-[:checked]:border-[var(--color-accent)]">
          <input type="radio" name="tier" value="priority" checked={tier === "priority"} onChange={() => setTier("priority")} className="accent-[var(--color-accent)]" />
          <span>
            <b className="text-[var(--color-fg)]">priority, ${price}</b>
            <span className="block text-xs text-[var(--color-dim)]">pay with CoinPay, live after the automated checks</span>
          </span>
        </label>
      </fieldset>
      {error && (
        <p role="alert" className="text-[var(--color-warn)]">
          {error}
          {existing && (
            <>
              {" "}
              <a href={`/gallery/status/${existing.id}`}>see its status</a>
            </>
          )}
        </p>
      )}
      <button
        type="submit"
        disabled={busy}
        className="!border px-5 py-2 rounded !border-[var(--color-accent)] text-[var(--color-accent)] hover:bg-[var(--color-accent-dim)]/30 disabled:opacity-50"
      >
        {busy ? "checking your site..." : tier === "priority" ? `submit and pay $${price}` : "submit"}
      </button>
      <p className="text-xs text-[var(--color-dim)]">We fetch the URL (it must answer 200), look up the domain&apos;s reputation and look for the badge.</p>
    </form>
  );
}
