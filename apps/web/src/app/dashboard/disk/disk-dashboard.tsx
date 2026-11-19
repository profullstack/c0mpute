"use client";

import { useCallback, useEffect, useMemo, useState } from "react";

// Mirrors lib/disk/server.ts DiskDashboard (kept loose: this is a client file).
type Totals = { earned_usd: number; spend_usd: number; own_usd: number; net_usd: number };
type Bucket = Totals & { t: number };
type Site = {
  site_id: string;
  name: string;
  url: string;
  via: "push" | "claim" | "owner";
  status: string | null;
  live: boolean;
  size_bytes: number | null;
  copies: number | null;
  price_per_gib_month: number;
  price_floor: boolean;
  holders: number;
  monthly_usd: number;
  spend_usd: number;
  own_usd: number;
};
type Node = {
  node: string;
  label: string | null;
  online: boolean;
  held_bytes: number;
  sites: number;
  earned_usd: number;
  own_usd: number;
  passed: number;
  challenged: number;
  offer: { free_bytes: number; total_bytes: number; per_gib_month: number } | null;
  app_earned_usd: number;
  app_own_usd: number;
  app_cpu_hours: number;
  apps: number;
};
type App = {
  app_id: string;
  name: string;
  url: string;
  via: "claim" | "owner";
  live: boolean;
  healthy: boolean | null;
  holders: number;
  hours: number;
  cpu_hours: number;
  ram_gib_hours: number;
  disk_gib: number;
  egress_bytes: number;
  ingress_bytes: number;
  cpu_usd: number;
  ram_usd: number;
  disk_usd: number;
  egress_usd: number;
  spend_usd: number;
  own_usd: number;
};
type Claim = { kind: "site" | "owner"; value: string; label: string | null };
type Dash = {
  range: string;
  from: number;
  to: number;
  bucket_ms: number;
  totals: Totals;
  buckets: Bucket[];
  sites: Site[];
  apps: App[];
  nodes: Node[];
  by_kind: { storage: Totals; apps: Totals };
  latest_hour: number | null;
  history_from: number | null;
  claims: Claim[];
};

const RANGES = ["1h", "3h", "24h", "7d", "30d"] as const;
// Same validated pair as /dashboard/nodes: earned green, spend violet.
const EARNED = "#4ade80";
const SPEND = "#c084fc";

function usd(n: number): string {
  if (n === 0) return "$0";
  const sign = n < 0 ? "-" : "";
  const a = Math.abs(n);
  return a < 0.01 ? `${sign}$${a.toPrecision(2)}` : `${sign}$${a.toFixed(2)}`;
}
function bytes(n: number | null): string {
  if (n == null) return "—";
  if (n < 1024 ** 2) return `${(n / 1024).toFixed(0)} KiB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MiB`;
  if (n < 1024 ** 4) return `${(n / 1024 ** 3).toFixed(2)} GiB`;
  return `${(n / 1024 ** 4).toFixed(2)} TiB`;
}
function tick(t: number, bucketMs: number): string {
  const d = new Date(t);
  if (bucketMs >= 86_400_000) return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
  if (bucketMs >= 6 * 3_600_000) return d.toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric" });
  return d.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
}

export function DiskDashboard() {
  const [me, setMe] = useState<{ name: string | null } | null | undefined>(undefined);
  const [range, setRange] = useState<(typeof RANGES)[number]>("24h");
  const [data, setData] = useState<Dash | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    try {
      const r = new URLSearchParams(window.location.search).get("range");
      if (r && (RANGES as readonly string[]).includes(r)) setRange(r as (typeof RANGES)[number]);
    } catch {}
    fetch("/api/v1/me")
      .then((r) => (r.ok ? r.json() : null))
      .then(setMe)
      .catch(() => setMe(null));
  }, []);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const r = await fetch(`/api/v1/disk?range=${range}`, { cache: "no-store" });
      if (!r.ok) throw new Error((await r.json().catch(() => null))?.error?.message ?? `HTTP ${r.status}`);
      setData(await r.json());
      setErr(null);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [range]);

  useEffect(() => {
    if (!me) return;
    load();
    try {
      const u = new URL(window.location.href);
      u.searchParams.set("range", range);
      window.history.replaceState(null, "", u);
    } catch {}
    const t = setInterval(load, 120_000);
    return () => clearInterval(t);
  }, [me, range, load]);

  if (me === undefined) return <p className="text-[var(--color-dim)] text-sm">loading…</p>;
  if (!me) {
    return (
      <div className="space-y-4 text-sm leading-7">
        <p>Sign in to see your hosted sites and apps, what they cost, and what your nodes earn hosting other people&apos;s. A CoinPay account with a DID is required.</p>
        <a
          href={`/api/auth/coinpay/login?next=${encodeURIComponent("/dashboard/disk")}`}
          className="inline-block !border border-[var(--color-accent)] px-4 py-2 accent"
        >
          Sign in with CoinPay
        </a>
      </div>
    );
  }

  return (
    <div className="space-y-8 text-sm">
      <div className="flex flex-wrap items-center gap-2" role="group" aria-label="time range">
        {RANGES.map((r) => (
          <button
            key={r}
            onClick={() => setRange(r)}
            aria-pressed={range === r}
            className={`px-3 py-1 border ${range === r ? "border-[var(--color-accent)] accent" : "border-[var(--color-rule)] text-[var(--color-dim)]"}`}
          >
            {r}
          </button>
        ))}
        <span className="text-xs text-[var(--color-dim)] ml-auto">
          {loading ? "updating…" : data ? `updated ${new Date(data.to).toLocaleTimeString()}` : ""}
        </span>
      </div>

      {err && <p className="text-[var(--color-warn)]">Could not load your storage numbers: {err}</p>}

      {data && (
        <>
          <Summary data={data} />
          <SitesTable data={data} onChange={load} />
          <AppsTable data={data} />
          <Chart data={data} />
          <NodesTable data={data} />
          <AddSite onDone={load} />
          <Notes data={data} />
        </>
      )}
    </div>
  );
}

function Summary({ data }: { data: Dash }) {
  const t = data.totals;
  const held = data.nodes.reduce((a, n) => a + n.held_bytes, 0);
  const stored = data.sites.reduce((a, s) => a + (s.size_bytes ?? 0), 0);
  const monthly = data.sites.reduce((a, s) => a + s.monthly_usd, 0);
  const k = data.by_kind;
  const cpuServed = data.nodes.reduce((a, n) => a + n.app_cpu_hours, 0);
  const tiles = [
    {
      k: "hosting earned",
      v: usd(t.earned_usd),
      sub: `disk ${usd(k.storage.earned_usd)} (holding ${bytes(held)}) · apps ${usd(k.apps.earned_usd)} (${cpuServed.toFixed(1)} CPU-h)`,
      cls: "accent",
    },
    {
      k: "hosting spend",
      v: usd(t.spend_usd),
      sub: `sites ${usd(k.storage.spend_usd)} (${data.sites.length}, ${bytes(stored)}, ~${usd(monthly)}/mo) · apps ${usd(k.apps.spend_usd)} (${data.apps.length})`,
      cls: "accent",
    },
    { k: "net", v: usd(t.net_usd), sub: t.net_usd >= 0 ? "earned minus spend" : "spending more than earning", cls: t.net_usd < 0 ? "text-[var(--color-warn)]" : "accent" },
    { k: "yours on your nodes", v: usd(t.own_usd), sub: "paid to yourself: neither", cls: "text-[var(--color-fg)]" },
  ];
  return (
    <section className="grid grid-cols-2 sm:grid-cols-4 gap-3" aria-label="totals">
      {tiles.map((x) => (
        <div key={x.k} className="border border-[var(--color-rule)] bg-[var(--color-card)] p-3 space-y-1">
          <div className="text-xs text-[var(--color-dim)]">{x.k}</div>
          <div className={`text-xl font-bold ${x.cls}`}>{x.v}</div>
          <div className="text-xs text-[var(--color-dim)]">{x.sub}</div>
        </div>
      ))}
      <p className="col-span-2 sm:col-span-4 text-xs text-[var(--color-dim)]">
        Sites: accrued by the hosting settlement rule; the owner&apos;s node pays its holders from its own wallet. Apps: metered by the node that ran them (CPU, RAM, disk, bandwidth) at its rates; not paid out yet. Window: {data.range}.
      </p>
    </section>
  );
}

function SitesTable({ data, onChange }: { data: Dash; onChange: () => void }) {
  const claimed = new Set(data.claims.map((c) => c.value));
  const remove = async (v: string) => {
    await fetch(`/api/v1/disk/sites?site=${encodeURIComponent(v)}`, { method: "DELETE" });
    onChange();
  };
  return (
    <section className="space-y-2" aria-label="your hosted sites">
      <h2 className="font-bold">your hosted sites</h2>
      {data.sites.length === 0 ? (
        <p className="text-[var(--color-dim)] leading-7">
          None yet. Sites you publish with <code>/push hosting</code> in <a href="/chat">chat</a> show up here by themselves. Published from a node with{" "}
          <code>c0mpute hosting publish</code>? Add its site id or your owner key below (<code>c0mpute hosting ls</code> shows both).
        </p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full min-w-[640px]">
            <thead className="text-xs text-[var(--color-dim)]">
              <tr>
                <th className="text-left font-normal py-1">site</th>
                <th className="text-right font-normal">size</th>
                <th className="text-right font-normal">copies</th>
                <th className="text-right font-normal">price</th>
                <th className="text-right font-normal">paid holders</th>
                <th className="text-right font-normal">~/month</th>
                <th className="text-right font-normal">spend ({data.range})</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {data.sites.map((s) => (
                <tr key={s.site_id} className="border-t border-[var(--color-rule)] align-top">
                  <td className="py-1.5">
                    <a href={s.url} target="_blank" rel="noopener" className="accent">
                      {s.name}
                    </a>
                    <div className="text-xs text-[var(--color-dim)]">
                      {s.site_id} · {s.via === "push" ? `chat push${s.status ? `, ${s.status}` : ""}` : s.via === "owner" ? "your owner key" : "added"}
                      {!s.live && " · not seen on the gateway"}
                    </div>
                  </td>
                  <td className="text-right">{bytes(s.size_bytes)}</td>
                  <td className="text-right">{s.copies ?? "—"}</td>
                  <td className="text-right">{`$${s.price_per_gib_month.toFixed(4)}/GiB-mo`}
                    {s.price_floor && <div className="text-xs text-[var(--color-dim)]">floor price</div>}</td>
                  <td className="text-right">{s.holders}</td>
                  <td className="text-right">{usd(s.monthly_usd)}</td>
                  <td className="text-right">
                    {usd(s.spend_usd)}
                    {s.own_usd > 0 && <div className="text-xs text-[var(--color-dim)]">+{usd(s.own_usd)} to your nodes</div>}
                  </td>
                  <td className="text-right">
                    {claimed.has(s.site_id) && (
                      <button onClick={() => remove(s.site_id)} className="text-xs text-[var(--color-dim)] hover:text-[var(--color-warn)]" aria-label={`remove ${s.name}`}>
                        remove
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {data.claims.some((c) => c.kind === "owner") && (
        <p className="text-xs text-[var(--color-dim)]">
          owner keys:{" "}
          {data.claims
            .filter((c) => c.kind === "owner")
            .map((c) => (
              <span key={c.value} className="mr-3">
                {c.value.slice(0, 12)}…{" "}
                <button onClick={() => remove(c.value)} className="hover:text-[var(--color-warn)]">
                  remove
                </button>
              </span>
            ))}
        </p>
      )}
    </section>
  );
}

function Chart({ data }: { data: Dash }) {
  const [hover, setHover] = useState<number | null>(null);
  const W = 720;
  const H = 200;
  const pad = { l: 60, r: 8, t: 10, b: 26 };
  const b = data.buckets;
  const max = useMemo(() => Math.max(1e-9, ...b.map((x) => Math.max(x.earned_usd, x.spend_usd))), [b]);
  const y = (v: number) => pad.t + (H - pad.t - pad.b) * (1 - v / max);
  const slot = (W - pad.l - pad.r) / b.length;
  const bw = Math.max(1, Math.min(14, (slot - 4) / 2));
  const x0 = (i: number) => pad.l + i * slot + slot / 2;
  const every = Math.ceil(b.length / 6);
  const h = hover !== null ? b[hover] : null;
  const empty = data.totals.earned_usd === 0 && data.totals.spend_usd === 0;
  return (
    <section className="space-y-2" aria-label="disk earned vs storage spend over time">
      <div className="flex flex-wrap items-center gap-4 text-xs text-[var(--color-dim)]">
        <span className="flex items-center gap-1">
          <span className="inline-block w-3 h-3 rounded-sm" style={{ background: EARNED }} /> disk earned
        </span>
        <span className="flex items-center gap-1">
          <span className="inline-block w-3 h-3 rounded-sm" style={{ background: SPEND }} /> storage spend
        </span>
      </div>
      <div className="relative overflow-x-auto">
        <svg viewBox={`0 0 ${W} ${H}`} className="w-full h-auto min-w-[560px]" role="img" aria-label="disk earned and storage spend per time bucket" onMouseLeave={() => setHover(null)}>
          {[0, max / 2, max].map((v, i) => (
            <g key={i}>
              <line x1={pad.l} x2={W - pad.r} y1={y(v)} y2={y(v)} stroke="var(--color-rule)" strokeWidth={1} />
              <text x={pad.l - 6} y={y(v) + 4} textAnchor="end" fontSize={10} fill="var(--color-dim)">
                {usd(v)}
              </text>
            </g>
          ))}
          {b.map((x, i) => (
            <g key={x.t}>
              {hover === i && <rect x={pad.l + i * slot} y={pad.t} width={slot} height={H - pad.t - pad.b} fill="var(--color-line)" />}
              <rect x={x0(i) - bw - 1} y={y(x.earned_usd)} width={bw} height={Math.max(0, y(0) - y(x.earned_usd))} rx={2} fill={EARNED} />
              <rect x={x0(i) + 1} y={y(x.spend_usd)} width={bw} height={Math.max(0, y(0) - y(x.spend_usd))} rx={2} fill={SPEND} />
              {i % every === 0 && (
                <text x={x0(i)} y={H - 8} textAnchor="middle" fontSize={10} fill="var(--color-dim)">
                  {tick(x.t, data.bucket_ms)}
                </text>
              )}
              <rect x={pad.l + i * slot} y={pad.t} width={slot} height={H - pad.t - pad.b} fill="transparent" onMouseEnter={() => setHover(i)} onTouchStart={() => setHover(i)} />
            </g>
          ))}
        </svg>
        {h && (
          <div className="absolute top-1 right-1 border border-[var(--color-rule)] bg-[var(--color-panel)] px-3 py-2 text-xs space-y-0.5 pointer-events-none">
            <div className="text-[var(--color-dim)]">{new Date(h.t).toLocaleString()}</div>
            <div>disk earned {usd(h.earned_usd)}</div>
            <div>storage spend {usd(h.spend_usd)}</div>
            {h.own_usd > 0 && <div className="text-[var(--color-dim)]">to yourself {usd(h.own_usd)}</div>}
          </div>
        )}
        {empty && (
          <p className="absolute inset-0 flex items-center justify-center text-xs text-[var(--color-dim)] pointer-events-none">
            nothing accrued in this window
          </p>
        )}
      </div>
    </section>
  );
}

function AppsTable({ data }: { data: Dash }) {
  if (data.apps.length === 0) {
    return (
      <section className="space-y-2" aria-label="your apps">
        <h2 className="font-bold">your apps</h2>
        <p className="text-[var(--color-dim)] leading-7">
          No apps yet. Deploy a backend with <code>c0mpute apps deploy</code>, then add its app id or your owner key below to see its CPU, RAM, disk and bandwidth bill.
        </p>
      </section>
    );
  }
  return (
    <section className="space-y-2" aria-label="your apps">
      <h2 className="font-bold">your apps</h2>
      <div className="overflow-x-auto">
        <table className="w-full min-w-[820px]">
          <thead className="text-xs text-[var(--color-dim)]">
            <tr>
              <th className="text-left font-normal py-1">app</th>
              <th className="text-right font-normal">CPU</th>
              <th className="text-right font-normal">RAM</th>
              <th className="text-right font-normal">disk</th>
              <th className="text-right font-normal">bandwidth</th>
              <th className="text-right font-normal">spend ({data.range})</th>
            </tr>
          </thead>
          <tbody>
            {data.apps.map((a) => (
              <tr key={a.app_id} className="border-t border-[var(--color-rule)] align-top">
                <td className="py-1.5">
                  <a href={a.url} target="_blank" rel="noopener" className="accent">
                    {a.name}
                  </a>
                  <div className="text-xs text-[var(--color-dim)]">
                    {a.app_id} · {a.live ? (a.healthy ? "healthy" : "not healthy") : "not seen on the gateway"} · {a.holders} copies · ran {a.hours.toFixed(1)} h
                  </div>
                </td>
                <td className="text-right">
                  {a.cpu_hours.toFixed(2)} CPU-h<div className="text-xs text-[var(--color-dim)]">{usd(a.cpu_usd)}</div>
                </td>
                <td className="text-right">
                  {a.ram_gib_hours.toFixed(2)} GiB-h<div className="text-xs text-[var(--color-dim)]">{usd(a.ram_usd)}</div>
                </td>
                <td className="text-right">
                  {a.disk_gib.toFixed(2)} GiB<div className="text-xs text-[var(--color-dim)]">{usd(a.disk_usd)}</div>
                </td>
                <td className="text-right">
                  {bytes(a.egress_bytes)} out<div className="text-xs text-[var(--color-dim)]">{bytes(a.ingress_bytes)} in · {usd(a.egress_usd)}</div>
                </td>
                <td className="text-right">
                  {usd(a.spend_usd)}
                  {a.own_usd > 0 && <div className="text-xs text-[var(--color-dim)]">+{usd(a.own_usd)} to your nodes</div>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function NodesTable({ data }: { data: Dash }) {
  return (
    <section className="space-y-2" aria-label="your nodes' disk">
      <h2 className="font-bold">your nodes: hosting</h2>
      {data.nodes.length === 0 ? (
        <p className="text-[var(--color-dim)] leading-7">
          No nodes on this account. Every node offers disk by default (and runs apps with the <code>apps</code> role); add yours on <a href="/dashboard/nodes">nodes: compute</a> and its hosting earnings show here.
        </p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full min-w-[820px]">
            <thead className="text-xs text-[var(--color-dim)]">
              <tr>
                <th className="text-left font-normal py-1">node</th>
                <th className="text-right font-normal">disk offered</th>
                <th className="text-right font-normal">free</th>
                <th className="text-right font-normal">paid data held</th>
                <th className="text-right font-normal">challenges</th>
                <th className="text-right font-normal">disk earned</th>
                <th className="text-right font-normal">apps run</th>
                <th className="text-right font-normal">apps earned</th>
              </tr>
            </thead>
            <tbody>
              {data.nodes.map((n) => (
                <tr key={n.node} className="border-t border-[var(--color-rule)]">
                  <td className="py-1.5">
                    {n.label ?? n.node}
                    <div className="text-xs text-[var(--color-dim)]">
                      {n.node} · {n.online ? "online" : "offline"}
                      {n.offer && ` · asks ${usd(n.offer.per_gib_month)}/GiB-mo`}
                    </div>
                  </td>
                  <td className="text-right">{n.offer ? bytes(n.offer.total_bytes) : "—"}</td>
                  <td className="text-right">{n.offer ? bytes(n.offer.free_bytes) : "—"}</td>
                  <td className="text-right">
                    {bytes(n.held_bytes)}
                    <div className="text-xs text-[var(--color-dim)]">{n.sites} sites</div>
                  </td>
                  <td className={`text-right ${n.challenged > 0 && n.passed * 2 < n.challenged ? "text-[var(--color-warn)]" : ""}`}>
                    {n.challenged ? `${n.passed}/${n.challenged}` : "—"}
                  </td>
                  <td className="text-right">
                    {usd(n.earned_usd)}
                    {n.own_usd > 0 && <div className="text-xs text-[var(--color-dim)]">+{usd(n.own_usd)} your own sites</div>}
                  </td>
                  <td className="text-right">
                    {n.apps}
                    <div className="text-xs text-[var(--color-dim)]">{n.app_cpu_hours.toFixed(1)} CPU-h</div>
                  </td>
                  <td className="text-right">
                    {usd(n.app_earned_usd)}
                    {n.app_own_usd > 0 && <div className="text-xs text-[var(--color-dim)]">+{usd(n.app_own_usd)} your own apps</div>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function AddSite({ onDone }: { onDone: () => void }) {
  const [v, setV] = useState("");
  const [msg, setMsg] = useState<string | null>(null);
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setMsg(null);
    const r = await fetch("/api/v1/disk/sites", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ site: v }) });
    if (r.ok) {
      setV("");
      onDone();
    } else setMsg((await r.json().catch(() => null))?.error?.message ?? `HTTP ${r.status}`);
  };
  return (
    <form onSubmit={submit} className="border border-[var(--color-rule)] p-4 space-y-2">
      <label htmlFor="add-site" className="font-bold block">
        add a site or app you published from a node
      </label>
      <p className="text-xs text-[var(--color-dim)]">
        A site or app id, its <code>/s/&lt;id&gt;/</code> or <code>/a/&lt;id&gt;/</code> link, or your 64-character owner key (adds every site and app you own, now and later). <code>c0mpute hosting ls</code> and <code>c0mpute apps ls</code> show them.
      </p>
      <div className="flex flex-wrap gap-2">
        <input
          id="add-site"
          value={v}
          onChange={(e) => setV(e.target.value)}
          placeholder="da5b7396b87dc287ae09"
          className="flex-1 min-w-0 bg-transparent border border-[var(--color-rule)] px-3 py-1"
        />
        <button className="!border border-[var(--color-accent)] px-4 py-1 accent">add</button>
      </div>
      {msg && <p className="text-xs text-[var(--color-warn)]">{msg}</p>}
    </form>
  );
}

function Notes({ data }: { data: Dash }) {
  return (
    <section className="text-xs text-[var(--color-dim)] space-y-2 leading-6">
      <h2 className="font-bold text-[var(--color-fg)] text-sm">how this is counted</h2>
      <p>
        Every site pays at least $0.004 per GiB-month (the floor; a site published without a price pays that). It owes each node holding it <code>price × GiB held × time / 30 days × challenges passed / asked</code>. A node that passes fewer than half its availability
        challenges in an hour is owed nothing for it.
      </p>
      <p>
        An app is metered by the node running it, about once a minute: CPU-hours, GiB-hours of RAM, GiB-months of disk (what it wrote plus its state) and GiB sent out
        (inbound is free), at that node&apos;s <code>[apps] price_*</code> rates (default $0.01/CPU-h, $0.002/GiB-h RAM, $0.004/GiB-mo disk, $0.01/GiB out). The node
        gossips one signed report per app and hour (<code>GET /apps/v1/usage</code>): its own account, attributable to it. Nothing is paid from it yet.
      </p>
      <p>
        These numbers are worked out from the public, signed receipts every gateway sees (<code>GET /hosting/v1/dues</code>) and recorded hour by hour.
        They say what is owed. The site owner&apos;s node pays it from its own CoinPay wallet once each settlement period; a node with <code>[payments]</code> off only logs it.
      </p>
      <p>
        {data.history_from ? `History starts ${new Date(data.history_from).toLocaleString()}.` : "No history recorded yet: the first hour lands about 10 minutes after it ends."}{" "}
        From a terminal: <code>c0mpute hosting money</code>. API: <code>GET /api/v1/disk?range=24h</code>.
      </p>
    </section>
  );
}
