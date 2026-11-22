"use client";

import { useCallback, useEffect, useRef, useState } from "react";

type Status = {
  coinpay: boolean;
  trial: { started_at: string; ends_at: string; days_left: number } | null;
  trial_available: boolean;
  subscriptions: { plan: string; paid_until: string; active: boolean }[];
  experts: { slug: string; state: string; allowed: boolean }[];
};
type Turn = { role: "user" | "assistant"; content: string; error?: boolean };

const card = "border border-[var(--color-rule)] bg-[var(--color-card)] rounded-lg p-4 sm:p-5 space-y-3";
const btn = "border border-[var(--color-accent)] px-3 py-1.5 accent hover:bg-[var(--color-accent-dim)] disabled:opacity-50";
const quiet = "border border-[var(--color-rule)] px-3 py-1.5 text-[var(--color-dim)] hover:text-[var(--color-fg)] disabled:opacity-50 text-left";
const day = (s: string) => s.slice(0, 10);

/** Trial / plan state, the subscribe form and a small chat with one expert. */
export function ExpertPanel(p: { slug: string; name: string; disclaimer: string | null; examples: string[]; price: number; bundlePrice: number; periodDays: number; trialDays: number }) {
  const [status, setStatus] = useState<Status | null | undefined>(undefined);
  const [turns, setTurns] = useState<Turn[]>([]);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [plan, setPlan] = useState<string>(p.slug);
  const [email, setEmail] = useState("");
  const [payErr, setPayErr] = useState<string | null>(null);
  const [paywall, setPaywall] = useState(false);
  const box = useRef<HTMLTextAreaElement>(null);

  const load = useCallback(async () => {
    const r = await fetch("/api/v1/experts/me");
    if (r.status === 401) return setStatus(null);
    if (r.ok) setStatus(await r.json());
  }, []);
  useEffect(() => {
    load();
    if (new URLSearchParams(location.search).get("paid") === "1") setTimeout(load, 4000);
  }, [load]);

  const mine = status?.experts.find((e) => e.slug === p.slug);
  const active = status?.subscriptions.filter((s) => s.active && (s.plan === p.slug || s.plan === "all")) ?? [];
  const next = `/experts/${p.slug}`;

  async function ask(text: string) {
    const q = text.trim();
    if (!q || busy) return;
    const history = [...turns.filter((t) => !t.error), { role: "user" as const, content: q }];
    setTurns([...turns, { role: "user", content: q }]);
    setInput("");
    setBusy(true);
    try {
      const r = await fetch(`/api/v1/experts/${p.slug}/ask`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ messages: history.slice(-12).map(({ role, content }) => ({ role, content })) }),
      });
      const j = await r.json().catch(() => ({}));
      if (r.status === 402) setPaywall(true);
      if (!r.ok) throw new Error(j?.error?.message || `HTTP ${r.status}`);
      const answer = j?.choices?.[0]?.message?.content || "(empty answer: try again or rephrase)";
      setTurns((t) => [...t, { role: "assistant", content: answer }]);
      load();
    } catch (e) {
      setTurns((t) => [...t, { role: "assistant", content: e instanceof Error ? e.message : String(e), error: true }]);
    } finally {
      setBusy(false);
    }
  }

  async function subscribe(e: React.FormEvent) {
    e.preventDefault();
    setPayErr(null);
    const r = await fetch("/api/v1/experts/checkout", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ plan, email }) });
    const j = await r.json().catch(() => ({}));
    if (r.ok && j.url) location.href = j.url;
    else setPayErr(j?.error?.message || `could not open checkout (HTTP ${r.status})`);
  }

  if (status === undefined) return <section className={card}>loading…</section>;

  if (status === null)
    return (
      <section className={card}>
        <h2 className="font-bold">Ask {p.name}</h2>
        <p className="text-sm">Sign in with CoinPay to start your {p.trialDays}-day free trial. One trial per CoinPay account, covering every expert.</p>
        <p>
          <a className={btn} href={`/api/auth/coinpay/login?next=${encodeURIComponent(next)}`}>
            sign in with CoinPay
          </a>
        </p>
        <Examples examples={p.examples} />
      </section>
    );

  if (!status.coinpay)
    return (
      <section className={card}>
        <h2 className="font-bold">Connect CoinPay</h2>
        <p className="text-sm">Experts trials and subscriptions are bound to a CoinPay account. Connect yours to start the {p.trialDays}-day free trial.</p>
        <p>
          <a className={btn} href={`/api/auth/coinpay/login?link=1&next=${encodeURIComponent(next)}`}>
            connect CoinPay
          </a>
        </p>
      </section>
    );

  const showPay = paywall || mine?.state === "expired" || (mine?.state === "trial" && (status.trial?.days_left ?? 99) <= 14);

  return (
    <>
      <section className={card} aria-live="polite">
        <h2 className="font-bold">Ask {p.name}</h2>
        <p className="text-sm text-[var(--color-dim)]">
          {active.length > 0 ? (
            <>Subscribed ({active[0].plan === "all" ? "all experts" : p.name}) until {day(active[0].paid_until)}.</>
          ) : mine?.state === "trial" && status.trial ? (
            <>
              Free trial: {status.trial.days_left} days left (ends {day(status.trial.ends_at)}).
            </>
          ) : mine?.state === "trial_available" ? (
            <>Your first question starts your {p.trialDays}-day free trial.</>
          ) : (
            <>Your free trial ended{status.trial ? ` on ${day(status.trial.ends_at)}` : ""}. Subscribe below to keep asking.</>
          )}
        </p>

        <ol className="space-y-3">
          {turns.map((t, i) => (
            <li key={i} className={t.role === "user" ? "text-[var(--color-dim)]" : ""}>
              <span className="text-xs accent">{t.role === "user" ? "you" : p.name.toLowerCase()}</span>
              <div className="whitespace-pre-wrap text-sm leading-6 break-words" style={t.error ? { color: "var(--color-warn)" } : undefined}>
                {t.content}
              </div>
              {t.role === "assistant" && !t.error && p.disclaimer && <p className="text-xs mt-1" style={{ color: "var(--color-warn)" }}>{p.disclaimer}</p>}
            </li>
          ))}
          {busy && <li className="text-sm text-[var(--color-dim)]">thinking on the network… (usually 10-90 s)</li>}
        </ol>

        <form
          onSubmit={(e) => {
            e.preventDefault();
            ask(input);
          }}
          className="space-y-2"
        >
          <label className="sr-only" htmlFor="expert-q">
            Your question
          </label>
          <textarea
            id="expert-q"
            ref={box}
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) ask(input);
            }}
            rows={4}
            maxLength={20000}
            placeholder={p.examples[0]}
            className="w-full bg-[var(--color-bg)] border border-[var(--color-rule)] p-2 text-sm"
          />
          <button className={btn} disabled={busy || !input.trim()}>
            {busy ? "asking…" : "ask"}
          </button>
        </form>
        {turns.length === 0 && (
          <Examples
            examples={p.examples}
            onPick={(q) => {
              setInput(q);
              box.current?.focus();
            }}
          />
        )}
      </section>

      {showPay && (
        <section id="subscribe" className={card}>
          <h2 className="font-bold">Subscribe</h2>
          <form onSubmit={subscribe} className="space-y-3 text-sm">
            <fieldset className="space-y-1">
              <legend className="sr-only">Plan</legend>
              <label className="flex gap-2 items-center">
                <input type="radio" name="plan" checked={plan === p.slug} onChange={() => setPlan(p.slug)} /> {p.name}: ${p.price} per {p.periodDays} days
              </label>
              <label className="flex gap-2 items-center">
                <input type="radio" name="plan" checked={plan === "all"} onChange={() => setPlan("all")} /> every expert: ${p.bundlePrice} per {p.periodDays} days
              </label>
            </fieldset>
            <label className="block">
              Receipt email{" "}
              <input type="email" required value={email} onChange={(e) => setEmail(e.target.value)} className="bg-[var(--color-bg)] border border-[var(--color-rule)] px-2 py-1 w-full sm:w-72" />
            </label>
            <button className={btn}>pay with CoinPay</button>
            {payErr && <p style={{ color: "var(--color-warn)" }}>{payErr}</p>}
            <p className="text-xs text-[var(--color-dim)]">Bound to your CoinPay account. Paying early adds to the time you have left.</p>
          </form>
        </section>
      )}
    </>
  );
}

function Examples({ examples, onPick }: { examples: string[]; onPick?: (q: string) => void }) {
  return (
    <div className="space-y-1">
      <p className="text-xs text-[var(--color-dim)]">example questions</p>
      <ul className="grid gap-1.5">
        {examples.map((q) => (
          <li key={q}>
            {onPick ? (
              <button type="button" className={`${quiet} w-full text-sm`} onClick={() => onPick(q)}>
                {q}
              </button>
            ) : (
              <span className="text-sm text-[var(--color-dim)]">· {q}</span>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}
