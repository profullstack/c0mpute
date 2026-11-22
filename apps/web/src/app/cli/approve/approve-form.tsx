"use client";

import { useEffect, useState } from "react";
import { SignInOptions } from "@/components/sign-in-options";

export function ApproveForm({ initialCode }: { initialCode: string }) {
  const [me, setMe] = useState<{ id: string; did: string | null; name: string | null } | null | undefined>(undefined);
  const [code, setCode] = useState(initialCode);
  const [state, setState] = useState<"idle" | "done" | "error">("idle");

  useEffect(() => {
    fetch("/api/v1/me")
      .then((r) => (r.ok ? r.json() : null))
      .then(setMe)
      .catch(() => setMe(null));
  }, []);

  if (me === undefined) return <p className="text-[var(--color-dim)]">loading…</p>;

  if (!me) {
    const next = `/cli/approve?code=${encodeURIComponent(code)}`;
    return (
      <div className="space-y-4">
        <p>Sign in to approve the code your terminal shows: CoinPay, a passkey, or a password all work.</p>
        <SignInOptions next={next} />
      </div>
    );
  }

  if (state === "done") {
    return <p className="accent">Approved. Go back to your terminal: the CLI is signed in.</p>;
  }

  return (
    <form
      className="space-y-4"
      onSubmit={async (e) => {
        e.preventDefault();
        const r = await fetch("/api/v1/cli/approve", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ code }),
        });
        setState(r.ok ? "done" : "error");
      }}
    >
      <p>
        Signed in as <span className="break-all">{me.name ? `${me.name} (${me.did ?? me.id})` : (me.did ?? me.id)}</span>. Only approve a
        code you started yourself with <code>c0mpute login</code>: it gives that terminal a key that acts as you (and can link nodes to your account).
      </p>
      <input
        value={code}
        onChange={(e) => setCode(e.target.value.toUpperCase())}
        placeholder="ABCD-EFGH"
        className="w-full bg-[var(--color-card)] border border-[var(--color-rule)] px-3 py-2 text-lg tracking-widest"
      />
      {state === "error" && <p className="text-[var(--color-warn)]">That code is unknown, used or expired. Run the login again.</p>}
      <button className="border border-[var(--color-accent)] px-4 py-2 accent">approve this terminal</button>
    </form>
  );
}
