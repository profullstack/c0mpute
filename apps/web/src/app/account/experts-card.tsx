"use client";

import { useEffect, useState } from "react";

type Status = {
  coinpay: boolean;
  trial: { started_at: string; ends_at: string; days_left: number } | null;
  trial_available: boolean;
  subscriptions: { plan: string; paid_until: string; active: boolean }[];
  usage: Record<string, number>;
  prices: { all_usd: number; period_days: number; trial_days: number };
};

const card = "border border-[var(--color-rule)] bg-[var(--color-card)] rounded-lg p-4 sm:p-5 space-y-3";
const day = (s: string) => s.slice(0, 10);

/** Experts on /account: the free trial, paid plans and questions asked. */
export function ExpertsCard() {
  const [s, setS] = useState<Status | null>(null);
  useEffect(() => {
    fetch("/api/v1/experts/me")
      .then((r) => (r.ok ? r.json() : null))
      .then(setS)
      .catch(() => {});
  }, []);
  if (!s) return null;
  const asked = Object.entries(s.usage);
  return (
    <section className={card}>
      <h2 className="font-bold">Experts</h2>
      {!s.coinpay ? (
        <p className="text-[var(--color-dim)]">Connect CoinPay above to start the {s.prices.trial_days}-day free Experts trial.</p>
      ) : s.trial ? (
        <p>
          Free trial: started {day(s.trial.started_at)}, {s.trial.days_left > 0 ? `${s.trial.days_left} days left (ends ${day(s.trial.ends_at)})` : `ended ${day(s.trial.ends_at)}`}.
        </p>
      ) : (
        <p>
          Not started: your first question to an <a href="/experts">expert</a> starts a {s.prices.trial_days}-day free trial.
        </p>
      )}
      {s.subscriptions.length > 0 && (
        <ul className="text-sm space-y-1">
          {s.subscriptions.map((x) => (
            <li key={x.plan}>
              {x.plan === "all" ? "all experts" : x.plan}: {x.active ? `paid until ${day(x.paid_until)}` : `ended ${day(x.paid_until)}`}
            </li>
          ))}
        </ul>
      )}
      {asked.length > 0 && <p className="text-sm text-[var(--color-dim)]">Questions asked: {asked.map(([k, n]) => `${k} ${n}`).join(", ")}</p>}
      <p className="text-sm">
        <a href="/experts">browse experts</a>
        <span className="text-[var(--color-dim)]"> · ${s.prices.all_usd} per {s.prices.period_days} days for all of them after the trial</span>
      </p>
    </section>
  );
}
