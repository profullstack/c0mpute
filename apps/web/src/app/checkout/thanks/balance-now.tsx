"use client";

import { useEffect, useState } from "react";

// A crypto payment lands after the chain confirms it: look again for about two
// minutes, then stop.
const EVERY_MS = 8_000;
const TRIES = 15;

/** The signed-in person's balance, re-read a few times after a payment. */
export function BalanceNow() {
  const [usd, setUsd] = useState<string | null>(null);
  const [state, setState] = useState<"checking" | "done" | "signed-out">("checking");

  useEffect(() => {
    let stop = false;
    let n = 0;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const read = async () => {
      n++;
      try {
        const res = await fetch("/api/v1/balance", { cache: "no-store", credentials: "same-origin" });
        if (stop) return;
        if (res.status === 401) return setState("signed-out");
        if (res.ok) {
          const body = (await res.json()) as { total_spendable_usd?: string | number };
          const value = Number(body.total_spendable_usd);
          if (!stop && Number.isFinite(value)) setUsd(value > 0 && value < 0.01 ? value.toFixed(6) : value.toFixed(2));
        }
      } catch {
        // offline or a deploy in progress: the next try may work
      }
      if (stop) return;
      if (n >= TRIES) return setState("done");
      timer = setTimeout(read, EVERY_MS);
    };
    read();
    return () => {
      stop = true;
      if (timer) clearTimeout(timer);
    };
  }, []);

  if (state === "signed-out") {
    return (
      <p className="border border-[var(--color-rule)] p-3 text-[var(--color-dim)]">
        <a href="/signin?next=%2Fcheckout%2Fthanks">Sign in</a> to see your balance.
      </p>
    );
  }
  return (
    <p className="border border-[var(--color-rule)] p-3" aria-live="polite">
      balance <span className="accent">{usd === null ? "…" : `$${usd}`}</span>
      {state === "checking" ? <span className="text-[var(--color-dim)]"> · checking for your payment</span> : null}
    </p>
  );
}
