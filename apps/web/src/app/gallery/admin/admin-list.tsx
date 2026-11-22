"use client";

import { useCallback, useEffect, useState } from "react";

type Row = {
  id: string;
  slug: string;
  url: string;
  name: string;
  tagline: string;
  category: string;
  tier: string;
  status: string;
  listed_via: string | null;
  featured: boolean;
  goes_live_at: string | null;
  paid: boolean;
  paid_at: string | null;
  refund_requested: boolean;
  reject_reason: string | null;
  contact_email: string | null;
  note: string | null;
  badge: { state: string | null; checked_at: string | null };
  created_at: string;
};

const STATUSES = ["all", "queued", "awaiting_payment", "priority_review", "review", "approved", "rejected"];

export function AdminList() {
  const [status, setStatus] = useState("all");
  const [rows, setRows] = useState<Row[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(async () => {
    setError(null);
    const res = await fetch(`/api/v1/gallery/admin?status=${status}`, { cache: "no-store" });
    const j = (await res.json().catch(() => ({}))) as { data?: Row[]; error?: { message?: string } };
    if (!res.ok) {
      setRows(null);
      setError(res.status === 401 ? "sign in at /chat first" : (j.error?.message ?? `HTTP ${res.status}`));
      return;
    }
    setRows(j.data ?? []);
  }, [status]);

  useEffect(() => {
    load();
  }, [load]);

  async function act(r: Row, action: string) {
    let reason: string | undefined;
    let refund = false;
    if (action === "reject") {
      reason = window.prompt(`Reject ${r.name}: reason (emailed to the submitter)`) ?? undefined;
      if (reason === undefined) return;
      if (r.paid) refund = window.confirm("They paid for priority. Flag a refund?");
    }
    if (action === "delete" && !window.confirm(`Delete ${r.url} for good?`)) return;
    setBusy(r.id);
    const res = await fetch(`/api/v1/gallery/admin/${r.id}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ action, reason, refund }),
    });
    const j = (await res.json().catch(() => ({}))) as { error?: { message?: string } };
    setBusy(null);
    if (!res.ok) setError(j.error?.message ?? `HTTP ${res.status}`);
    load();
  }

  const btn = "px-2 py-0.5 rounded border border-[var(--color-rule)] hover:border-[var(--color-accent)] disabled:opacity-40";

  return (
    <div className="space-y-4">
      <nav className="flex flex-wrap gap-2" aria-label="filter">
        {STATUSES.map((s) => (
          <button key={s} type="button" onClick={() => setStatus(s)} className={`${btn} ${s === status ? "!border-[var(--color-accent)] text-[var(--color-accent)]" : ""}`}>
            {s}
          </button>
        ))}
      </nav>
      {error && <p role="alert" className="text-[var(--color-warn)]">{error}</p>}
      {rows && rows.length === 0 && <p className="text-[var(--color-dim)]">nothing here</p>}
      <ul className="space-y-3">
        {rows?.map((r) => (
          <li key={r.id} className="rounded border border-[var(--color-rule)] bg-[var(--color-card)] p-4 space-y-2">
            <div className="flex flex-wrap items-baseline gap-x-3">
              <b className="text-[var(--color-fg)]">{r.name}</b>
              <a href={r.url} className="break-all">{r.url}</a>
              <span className="text-[var(--color-dim)]">
                {r.status} · {r.tier}
                {r.listed_via ? ` · via ${r.listed_via}` : ""}
                {r.featured ? " · featured" : ""}
                {r.paid ? ` · paid ${r.paid_at?.slice(0, 10)}` : ""}
                {r.refund_requested ? " · REFUND FLAGGED" : ""}
              </span>
            </div>
            {r.tagline && <p>{r.tagline}</p>}
            <p className="text-xs text-[var(--color-dim)] break-words">
              {r.category} · {r.contact_email ?? "no email"} · badge {r.badge.state ?? "unchecked"} · submitted {r.created_at.slice(0, 16)}Z
              {r.goes_live_at ? ` · goes live ${r.goes_live_at.slice(0, 10)}` : ""}
              {r.note ? ` · ${r.note}` : ""}
            </p>
            <div className="flex flex-wrap gap-2 text-xs">
              {r.status !== "approved" && <button type="button" className={btn} disabled={busy === r.id} onClick={() => act(r, "approve")}>approve</button>}
              {r.status !== "rejected" && <button type="button" className={btn} disabled={busy === r.id} onClick={() => act(r, "reject")}>reject</button>}
              <button type="button" className={btn} disabled={busy === r.id} onClick={() => act(r, r.featured ? "unfeature" : "feature")}>{r.featured ? "unfeature" : "feature"}</button>
              {r.status === "approved" && <button type="button" className={btn} disabled={busy === r.id} onClick={() => act(r, "requeue")}>requeue</button>}
              <a className={btn} href={`/gallery/status/${r.id}`}>status page</a>
              <button type="button" className={`${btn} ml-auto`} disabled={busy === r.id} onClick={() => act(r, "delete")}>delete</button>
            </div>
          </li>
        ))}
      </ul>
    </div>
  );
}
