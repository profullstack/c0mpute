"use client";

import { useCallback, useEffect, useState } from "react";

type Kinds = { offline: boolean; earnings: boolean; update: boolean; disk: boolean };
type Settings = {
  email: string | null;
  email_verified: boolean;
  sms: boolean;
  sms_available: boolean;
  offline_minutes: number;
  earnings_drop_pct: number;
  earnings_min_usd: number;
  disk_min_gb: number;
  disk_min_pct: number;
  cooldown_minutes: number;
  kinds: Kinds;
};
type View = {
  settings: Settings;
  active: { node: string; kind: string; since: string | null; detail: string }[];
  log: { node: string; kind: string; event: string; channels: string[]; detail: string; ok: boolean; at: string }[];
};

const card = "border border-[var(--color-rule)] bg-[var(--color-card)] rounded-lg p-4 sm:p-5 space-y-3";
const btn = "border border-[var(--color-accent)] px-3 py-1.5 accent hover:bg-[var(--color-accent-dim)] disabled:opacity-50";
const quiet = "border border-[var(--color-rule)] px-3 py-1.5 text-[var(--color-dim)] hover:text-[var(--color-fg)] disabled:opacity-50";
const field = "bg-[var(--color-bg)] border border-[var(--color-rule)] px-2 py-1 w-24";
const KIND_LABEL: Record<keyof Kinds, string> = { offline: "node offline", earnings: "earnings drop", update: "update failed", disk: "disk low" };
const CONFIRM_MSG: Record<string, string> = {
  confirmed: "Alert email confirmed. Alerts will go there.",
  "link-expired": "That confirmation link is not valid any more. Save the address again for a new one.",
  "slow-down": "Too many tries; wait a bit and open the link again.",
  error: "Could not confirm right now. Try the link again in a moment.",
};

async function call(method: string, body?: unknown) {
  const r = await fetch("/api/v1/fleet/alerts", { method, headers: body ? { "content-type": "application/json" } : {}, body: body ? JSON.stringify(body) : undefined });
  const j = await r.json().catch(() => ({}));
  if (!r.ok && !(r.status === 409 && j?.detail)) throw new Error(j?.error?.message || `HTTP ${r.status}`);
  return j;
}

/** Fleet alerts on /account: where they go, thresholds, which kinds, what is open, what was sent. */
export function AlertsCard() {
  const [view, setView] = useState<View | null>(null);
  const [msg, setMsg] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      setView(await call("GET"));
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  }, []);
  useEffect(() => {
    load();
    const q = new URLSearchParams(window.location.search).get("alerts");
    if (q && CONFIRM_MSG[q]) (q === "confirmed" ? setMsg : setErr)(CONFIRM_MSG[q]);
  }, [load]);

  const save = async (patch: Record<string, unknown>) => {
    setBusy(true);
    setErr(null);
    setMsg(null);
    try {
      const r = await call("PUT", patch);
      setView(r);
      setMsg(r.notice ?? "Saved.");
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  if (!view) return <section id="alerts" className={card}><h2 className="font-bold">alerts</h2><p className="text-[var(--color-dim)]">{err ?? "loading…"}</p></section>;
  const s = view.settings;
  return (
    <section id="alerts" className={card}>
      <h2 className="font-bold">alerts</h2>
      <p className="text-[var(--color-dim)]">
        We email you when a node goes offline, your fleet&apos;s earnings drop, an update fails or a disk runs low: one email when it starts and one when it is fixed. Same settings from the CLI (<code>c0mpute fleet alerts</code>), the API (<code>/api/v1/fleet/alerts</code>) and MCP (<code>fleet_alerts_get</code>, <code>fleet_alerts_set</code>).
      </p>
      {msg && <p className="accent">{msg}</p>}
      {err && <p className="text-[var(--color-warn)]">{err}</p>}

      <form
        className="flex flex-wrap items-center gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          const email = String(new FormData(e.currentTarget).get("email") || "").trim();
          save({ email: email || null });
        }}
      >
        <label htmlFor="alert-email">email</label>
        <input id="alert-email" name="email" type="email" defaultValue={s.email ?? ""} placeholder="you@example.com" className="bg-[var(--color-bg)] border border-[var(--color-rule)] px-2 py-1 min-w-0 flex-1 sm:flex-none sm:w-72" />
        <button className={btn} disabled={busy}>
          save
        </button>
        {s.email && <span className={s.email_verified ? "accent" : "text-[var(--color-warn)]"}>{s.email_verified ? "confirmed" : "not confirmed yet: open the link we emailed"}</span>}
      </form>

      <fieldset className="flex flex-wrap gap-x-5 gap-y-2">
        <legend className="sr-only">alert kinds</legend>
        {(Object.keys(KIND_LABEL) as (keyof Kinds)[]).map((k) => (
          <label key={k} className="flex items-center gap-1.5">
            <input type="checkbox" checked={s.kinds[k]} disabled={busy} onChange={(e) => save({ kinds: { [k]: e.target.checked } })} />
            {KIND_LABEL[k]}
          </label>
        ))}
        {s.sms_available && (
          <label className="flex items-center gap-1.5">
            <input type="checkbox" checked={s.sms} disabled={busy} onChange={(e) => save({ sms: e.target.checked })} />
            SMS (fleet alert line)
          </label>
        )}
      </fieldset>

      <form
        className="grid grid-cols-1 sm:grid-cols-2 gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          const f = new FormData(e.currentTarget);
          const patch: Record<string, number> = {};
          for (const k of ["offline_minutes", "earnings_drop_pct", "earnings_min_usd", "disk_min_gb", "disk_min_pct", "cooldown_minutes"]) patch[k] = Number(f.get(k));
          save(patch);
        }}
      >
        <label className="flex items-center gap-2">
          offline after <input name="offline_minutes" type="number" min={5} max={1440} defaultValue={s.offline_minutes} className={field} /> min
        </label>
        <label className="flex items-center gap-2">
          earnings drop <input name="earnings_drop_pct" type="number" min={10} max={100} defaultValue={s.earnings_drop_pct} className={field} /> % vs 7-day avg
        </label>
        <label className="flex items-center gap-2">
          ignore under $<input name="earnings_min_usd" type="number" min={0} step="0.01" defaultValue={s.earnings_min_usd} className={field} /> /day
        </label>
        <label className="flex items-center gap-2">
          disk low under <input name="disk_min_gb" type="number" min={0} step="1" defaultValue={s.disk_min_gb} className={field} /> GB
        </label>
        <label className="flex items-center gap-2">
          or under <input name="disk_min_pct" type="number" min={0} max={90} step="0.5" defaultValue={s.disk_min_pct} className={field} /> % free
        </label>
        <label className="flex items-center gap-2">
          cooldown <input name="cooldown_minutes" type="number" min={0} max={1440} defaultValue={s.cooldown_minutes} className={field} /> min
        </label>
        <div className="flex gap-2 sm:col-span-2">
          <button className={btn} disabled={busy}>
            save thresholds
          </button>
          <button
            type="button"
            className={quiet}
            disabled={busy || !s.email_verified}
            onClick={async () => {
              setErr(null);
              setMsg(null);
              try {
                const r = await call("POST", { test: true });
                (r.ok ? setMsg : setErr)(r.detail);
                load();
              } catch (e) {
                setErr(e instanceof Error ? e.message : String(e));
              }
            }}
          >
            send a test
          </button>
        </div>
      </form>

      <div>
        <h3 className="font-bold">open now</h3>
        {view.active.length === 0 ? (
          <p className="text-[var(--color-dim)]">nothing</p>
        ) : (
          <ul className="divide-y divide-[var(--color-line)]">
            {view.active.map((a) => (
              <li key={`${a.node}-${a.kind}`} className="py-1">
                <span className="text-[var(--color-warn)]">{a.kind}</span> {a.node === "*" ? "fleet" : <code>{a.node}</code>} <span className="text-[var(--color-dim)]">since {a.since?.slice(0, 16).replace("T", " ")}: {a.detail}</span>
              </li>
            ))}
          </ul>
        )}
      </div>
      {view.log.length > 0 && (
        <details>
          <summary className="cursor-pointer">recent notices ({view.log.length})</summary>
          <ul className="divide-y divide-[var(--color-line)] text-xs">
            {view.log.map((l, i) => (
              <li key={i} className="py-1">
                {l.at.slice(0, 16).replace("T", " ")} · {l.event} · {l.kind} · {l.node === "*" ? "fleet" : l.node} · {l.channels.join("+") || "not sent"} · <span className="text-[var(--color-dim)]">{l.detail}</span>
              </li>
            ))}
          </ul>
        </details>
      )}
    </section>
  );
}
