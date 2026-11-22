"use client";

import { useState } from "react";

export function PasswordForm({ next }: { next: string }) {
  const [mode, setMode] = useState<"login" | "signup">("login");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  return (
    <form
      className="space-y-4"
      onSubmit={async (e) => {
        e.preventDefault();
        setErr(null);
        setBusy(true);
        try {
          const r = await fetch(`/api/auth/password/${mode}`, {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ username, password }),
          });
          const j = await r.json().catch(() => ({}));
          if (!r.ok) throw new Error(j?.error?.message || `HTTP ${r.status}`);
          // Straight to the account page's "secure your account" prompt, then on.
          window.location.href = `/account?secure=1&next=${encodeURIComponent(next)}`;
        } catch (e) {
          setErr(e instanceof Error ? e.message : String(e));
        } finally {
          setBusy(false);
        }
      }}
    >
      <label className="block space-y-1">
        <span>username</span>
        <input
          autoComplete="username"
          value={username}
          onChange={(e) => setUsername(e.target.value)}
          className="w-full bg-[var(--color-card)] border border-[var(--color-rule)] px-3 py-2"
          required
          minLength={3}
          maxLength={32}
        />
      </label>
      <label className="block space-y-1">
        <span>password {mode === "signup" && <span className="text-[var(--color-dim)]">(12+ characters; checked against known breaches)</span>}</span>
        <input
          type="password"
          autoComplete={mode === "login" ? "current-password" : "new-password"}
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          className="w-full bg-[var(--color-card)] border border-[var(--color-rule)] px-3 py-2"
          required
          minLength={mode === "signup" ? 12 : 1}
        />
      </label>
      {err && <p className="text-[var(--color-warn)]">{err}</p>}
      <div className="flex items-center gap-4">
        <button disabled={busy} className="border border-[var(--color-rule)] px-4 py-2 disabled:opacity-50">
          {mode === "login" ? "sign in with password" : "create account with password"}
        </button>
        <button type="button" className="underline text-[var(--color-dim)]" onClick={() => setMode(mode === "login" ? "signup" : "login")}>
          {mode === "login" ? "no account? create one" : "have an account? sign in"}
        </button>
      </div>
    </form>
  );
}
