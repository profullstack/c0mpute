"use client";

import { useCallback, useEffect, useState } from "react";
import { ReferralTerms } from "@/components/referral-terms";

// Mirrors Infernet's GET /api/v1/referrals (kept loose: this is a client file).
type Balance = { pending_usd: number; confirmed_usd: number; requested_usd: number; paid_usd: number; available_usd: number; commissions: number };
type Overview = {
  has_code: boolean;
  code?: string;
  terms: Parameters<typeof ReferralTerms>[0]["terms"];
  links?: { c0mpute: string; infernet: string };
  install?: { c0mpute: string; c0mpute_register: string; infernet: string; infernet_register: string };
  payout_address?: { coin: string; network: string; address: string } | null;
  nodes?: { node: string; via: string; attributed_at: string; first_paid_job_at: string | null }[];
  referred_nodes?: number;
  active_nodes_7d?: number;
  balance?: Balance;
  recent?: { id: string; node: string; fee_usd: number; amount_usd: number; status: string; earned_at: string }[];
  payouts?: { id: string; amount_usd: number; coin: string; status: string; created_at: string; tx_hash: string | null; error: string | null }[];
};

function usd(n: number | undefined): string {
  const v = Number(n) || 0;
  if (v === 0) return "$0";
  return v < 0.01 ? `$${v.toFixed(6)}` : `$${v.toFixed(2)}`;
}

function Cmd({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex items-start gap-2">
      <pre className="flex-1 !border border-[var(--color-rule)] px-3 py-2 text-xs whitespace-pre-wrap break-all">{text}</pre>
      <button
        type="button"
        className="!border border-[var(--color-rule)] px-2 py-1 text-xs text-[var(--color-dim)]"
        onClick={() => navigator.clipboard?.writeText(text).then(() => { setCopied(true); setTimeout(() => setCopied(false), 1500); })}
      >
        {copied ? "copied" : "copy"}
      </button>
    </div>
  );
}

async function call(path: string, init?: RequestInit) {
  const r = await fetch(path, { cache: "no-store", ...init, headers: { "content-type": "application/json", ...(init?.headers ?? {}) } });
  const body = await r.json().catch(() => ({}));
  if (!r.ok) {
    const e = body?.error;
    throw new Error(typeof e === "string" ? e : e?.message || `HTTP ${r.status}`);
  }
  return body?.data ?? body;
}

export function ReferDashboard() {
  const [signedIn, setSignedIn] = useState<boolean | undefined>(undefined);
  const [o, setO] = useState<Overview | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [addr, setAddr] = useState({ coin: "USDC", network: "polygon", address: "" });
  const [amount, setAmount] = useState("");

  const load = useCallback(async () => {
    try {
      const d = (await call("/api/v1/referrals")) as Overview;
      setO(d);
      if (d.payout_address) setAddr(d.payout_address);
      setErr(null);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    fetch("/api/v1/me", { cache: "no-store" })
      .then((r) => {
        setSignedIn(r.ok);
        if (r.ok) load();
      })
      .catch(() => setSignedIn(false));
  }, [load]);

  const act = async (fn: () => Promise<unknown>, done: string) => {
    setBusy(true);
    setNote(null);
    try {
      await fn();
      setNote(done);
      await load();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  if (signedIn === undefined) return <p className="text-[var(--color-dim)] text-sm">loading…</p>;
  if (!signedIn) {
    return (
      <div className="space-y-4 text-sm leading-7">
        <a href={`/api/auth/coinpay/login?next=${encodeURIComponent("/dashboard/refer")}`} className="inline-block !border border-[var(--color-accent)] px-4 py-2 accent">
          sign in with CoinPay
        </a>
        <p className="text-[var(--color-dim)]">Sign in to get your referral link.</p>
      </div>
    );
  }

  const b = o?.balance;
  return (
    <div className="space-y-8 text-sm">
      {err && <p className="text-[var(--color-warn)]">{err}</p>}
      {note && <p className="accent">{note}</p>}
      {o && !o.has_code && (
        <section className="space-y-3">
          <p className="leading-7">
            When a node you referred serves paid jobs, you get {o.terms?.commission_pct_of_house_fee ?? 5}% of the house fee on each
            one. It comes out of the house&apos;s cut, never the operator&apos;s earnings. One link works on c0mpute.com and
            infernetprotocol.com.
          </p>
          <button type="button" disabled={busy} className="!border border-[var(--color-accent)] px-4 py-2 accent" onClick={() => act(() => call("/api/v1/referrals", { method: "POST", body: "{}" }), "your referral link is ready")}>
            create my referral link
          </button>
        </section>
      )}
      {o?.has_code && (
        <>
          <section className="space-y-2">
            <h2 className="text-lg font-bold accent">your link</h2>
            <p className="text-[var(--color-dim)]">
              code <span className="accent">{o.code}</span> · your commission term: {o.terms?.term?.label}
            </p>
            {o.links && <Cmd text={o.links.c0mpute} />}
            {o.links && <Cmd text={o.links.infernet} />}
            <p className="comment">// a node is credited to you when it registers with your code, before its first paid job</p>
            {o.install && (
              <>
                <Cmd text={o.install.c0mpute} />
                <Cmd text={o.install.c0mpute_register} />
                <Cmd text={o.install.infernet} />
              </>
            )}
          </section>
          <section className="grid grid-cols-2 sm:grid-cols-4 gap-3">
            {[
              ["referred nodes", String(o.referred_nodes ?? 0)],
              ["active (7 days)", String(o.active_nodes_7d ?? 0)],
              ["pending", usd(b?.pending_usd)],
              ["available", usd(b?.available_usd)],
              ["confirmed", usd(b?.confirmed_usd)],
              ["requested", usd(b?.requested_usd)],
              ["paid out", usd(b?.paid_usd)],
              ["commissions", String(b?.commissions ?? 0)],
            ].map(([k, v]) => (
              <div key={k} className="!border border-[var(--color-rule)] p-3">
                <p className="text-xs text-[var(--color-dim)]">{k}</p>
                <p className="text-lg accent">{v}</p>
              </div>
            ))}
          </section>
          <section className="space-y-3">
            <h2 className="text-lg font-bold accent">payouts</h2>
            <p className="text-[var(--color-dim)]">
              {o.payout_address ? `paid to ${o.payout_address.coin} on ${o.payout_address.network}: ${o.payout_address.address}` : "set where commissions are paid first"}
            </p>
            <div className="flex flex-wrap gap-2">
              <input aria-label="coin" className="!border border-[var(--color-rule)] bg-transparent px-2 py-1 w-24" value={addr.coin} onChange={(e) => setAddr({ ...addr, coin: e.target.value })} />
              <input aria-label="network" className="!border border-[var(--color-rule)] bg-transparent px-2 py-1 w-28" value={addr.network} onChange={(e) => setAddr({ ...addr, network: e.target.value })} />
              <input aria-label="address" placeholder="0x..." className="!border border-[var(--color-rule)] bg-transparent px-2 py-1 flex-1 min-w-[12rem]" value={addr.address} onChange={(e) => setAddr({ ...addr, address: e.target.value })} />
              <button type="button" disabled={busy} className="!border border-[var(--color-rule)] px-3 py-1" onClick={() => act(() => call("/api/v1/referrals/address", { method: "POST", body: JSON.stringify(addr) }), "payout address saved")}>
                save
              </button>
            </div>
            <div className="flex flex-wrap gap-2">
              <input aria-label="amount" placeholder={`all (${usd(b?.available_usd)})`} className="!border border-[var(--color-rule)] bg-transparent px-2 py-1 w-40" value={amount} onChange={(e) => setAmount(e.target.value)} />
              <button type="button" disabled={busy || !o.payout_address} className="!border border-[var(--color-accent)] px-3 py-1 accent" onClick={() => act(() => call("/api/v1/referrals/payout", { method: "POST", body: JSON.stringify(amount.trim() ? { amount_usd: Number(amount) } : {}) }), "payout requested")}>
                request payout
              </button>
            </div>
            {(o.payouts ?? []).length > 0 && (
              <ul className="text-xs text-[var(--color-dim)] space-y-1">
                {(o.payouts ?? []).map((p) => (
                  <li key={p.id}>
                    {p.created_at.slice(0, 10)} · {usd(p.amount_usd)} {p.coin} · {p.status}
                    {p.tx_hash ? ` · ${p.tx_hash.slice(0, 12)}…` : ""}
                    {p.error ? ` · ${p.error}` : ""}
                  </li>
                ))}
              </ul>
            )}
          </section>
          <section className="space-y-2">
            <h2 className="text-lg font-bold accent">referred nodes</h2>
            {(o.nodes ?? []).length === 0 ? (
              <p className="text-[var(--color-dim)]">none yet</p>
            ) : (
              <ul className="space-y-1 text-[var(--color-dim)]">
                {(o.nodes ?? []).map((n) => (
                  <li key={n.node + n.attributed_at}>
                    <span className="accent">{n.node}</span> · joined {n.attributed_at.slice(0, 10)}
                    {n.first_paid_job_at ? ` · first paid job ${n.first_paid_job_at.slice(0, 10)}` : " · no paid jobs yet"}
                  </li>
                ))}
              </ul>
            )}
            {(o.recent ?? []).length > 0 && (
              <table className="w-full text-xs text-left">
                <thead className="text-[var(--color-dim)]">
                  <tr><th className="py-1">when</th><th>node</th><th>house fee</th><th>commission</th><th>status</th></tr>
                </thead>
                <tbody>
                  {(o.recent ?? []).map((c) => (
                    <tr key={c.id}>
                      <td className="py-1">{c.earned_at.slice(0, 16).replace("T", " ")}</td>
                      <td>{c.node}</td>
                      <td>{usd(c.fee_usd)}</td>
                      <td className="accent">{usd(c.amount_usd)}</td>
                      <td>{c.status}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </section>
        </>
      )}
      {o && <ReferralTerms terms={o.terms} />}
    </div>
  );
}
