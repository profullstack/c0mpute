"use client";

import { useEffect, useState } from "react";

/** "Hide me from the leaderboard": one checkbox, for the signed-in account. */
export function LeaderboardPrivacy() {
  const [hidden, setHidden] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    fetch("/api/v1/me/leaderboard", { cache: "no-store" })
      .then((r) => (r.ok ? r.json() : null))
      .then((d) => setHidden(typeof d?.hidden === "boolean" ? d.hidden : null))
      .catch(() => setHidden(null));
  }, []);

  if (hidden === null) return null; // signed out, or the setting is not answering

  const toggle = async (next: boolean) => {
    setBusy(true);
    setErr(null);
    try {
      const r = await fetch("/api/v1/me/leaderboard", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ hidden: next }),
      });
      if (!r.ok) throw new Error(`HTTP ${r.status}`);
      setHidden(next);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="!border border-[var(--color-rule)] p-4 space-y-2 text-sm">
      <label className="flex items-start gap-3">
        <input type="checkbox" checked={hidden} disabled={busy} onChange={(e) => toggle(e.target.checked)} className="mt-1" />
        <span>
          <strong>Hide me from the <a href="/leaderboard">leaderboard</a></strong>
          <span className="block text-xs text-[var(--color-dim)] leading-5">
            The leaderboard never shows your email, DID, wallet or account id: you appear only as an
            anon-xxxxxx handle, and your nodes by their public id or nickname. Tick this to leave it out
            entirely: your handle, and the nodes you claimed from the node itself. Takes effect within 5
            minutes. Your nodes stay on /nodes, which lists every node online.
          </span>
        </span>
      </label>
      {err && <p className="text-xs text-[var(--color-warn)]">could not save: {err}</p>}
    </section>
  );
}
