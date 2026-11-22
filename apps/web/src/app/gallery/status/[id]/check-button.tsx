"use client";

import { useState } from "react";

export function CheckButton({ id }: { id: string }) {
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);

  async function check() {
    setBusy(true);
    setMsg(null);
    try {
      const res = await fetch(`/api/v1/gallery/submissions/${id}/check`, { method: "POST" });
      const j = (await res.json().catch(() => ({}))) as { message?: string; error?: { message?: string }; listing?: { status?: string } };
      setMsg(j.message ?? j.error?.message ?? `HTTP ${res.status}`);
      if (j.listing?.status === "approved") setTimeout(() => window.location.reload(), 1200);
    } catch {
      setMsg("could not reach c0mpute.com; try again");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-2">
      <button
        type="button"
        onClick={check}
        disabled={busy}
        className="!border px-5 py-2 rounded !border-[var(--color-accent)] text-[var(--color-accent)] hover:bg-[var(--color-accent-dim)]/30 disabled:opacity-50"
      >
        {busy ? "checking..." : "check my badge now"}
      </button>
      {msg && (
        <p role="status" className="text-sm">
          {msg}
        </p>
      )}
    </div>
  );
}
