"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { SignInOptions } from "@/components/sign-in-options";

// Mirrors lib/earnings/server.ts Dashboard (kept loose: this is a client file).
type Totals = {
  jobs: number;
  busy_s: number;
  earned_usd: number;
  paid_jobs: number;
  own_usd: number;
  own_jobs: number;
  agreed_usd: number;
  spend_usd: number;
  net_usd: number;
};
type Bucket = Totals & { t: number };
type NodeOut = Totals & {
  node: string;
  label: string | null;
  class: string;
  online: boolean;
  via: string;
  kind: "c0mpute" | "infernet";
  spend_basis: "fixed" | "none";
  cost_usd_per_hour: number | null;
  utilization: number;
};
type Dashboard = {
  range: string;
  from: number;
  to: number;
  bucket_ms: number;
  totals: Totals;
  buckets: Bucket[];
  nodes: NodeOut[];
  by_type: { type: string; jobs: number; busy_s: number; agreed_usd: number }[];
  infernet: { linked: boolean; providers: number } | null;
  history_from: number | null;
};

// The part of /api/v1/disk this page uses: hosting (sites, apps, disk) money.
type HostingTotals = { earned_usd: number; spend_usd: number; own_usd: number; net_usd: number };
type Hosting = {
  totals: HostingTotals;
  by_kind: { storage: HostingTotals; apps: HostingTotals };
  sites: unknown[];
  apps: unknown[];
  nodes: { node: string; earned_usd: number; own_usd: number; held_bytes: number; app_earned_usd: number; app_own_usd: number; app_cpu_hours: number; apps: number }[];
};

const RANGES = ["1h", "3h", "24h", "7d", "30d"] as const;
// Validated pair on the site's dark surface (dataviz validator, CVD ΔE 23):
// earned = the site's green, spend = violet. Identity also by legend + table.
const EARNED = "#4ade80";
const SPEND = "#c084fc";

function usd(n: number): string {
  if (n === 0) return "$0";
  const sign = n < 0 ? "-" : "";
  const a = Math.abs(n);
  return a < 0.01 ? `${sign}$${a.toPrecision(2)}` : `${sign}$${a.toFixed(2)}`;
}
function duration(s: number): string {
  if (s < 90) return `${Math.round(s)}s`;
  if (s < 5400) return `${(s / 60).toFixed(1)}m`;
  return `${(s / 3600).toFixed(1)}h`;
}
function jobLabel(t: string): string {
  const known: Record<string, string> = {
    "infernet.inference": "chat / inference",
    "infernet.provider": "Infernet provider jobs",
    "tts.synthesize": "text to speech",
    "stt.transcribe": "speech to text",
    "diffusion.txt2img": "images",
    "music.txt2music": "music",
    "video.txt2vid": "video",
    "video.img2vid": "image to video",
    "ffmpeg.transcode": "transcode",
    "workflow.step": "workflow step",
    "container.run": "container",
    unknown: "other",
  };
  return known[t] ?? t;
}
function tick(t: number, bucketMs: number): string {
  const d = new Date(t);
  if (bucketMs >= 86_400_000) return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
  if (bucketMs >= 6 * 3_600_000) return d.toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric" });
  return d.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
}

export function EarningsDashboard() {
  const [me, setMe] = useState<{ name: string | null; did: string } | null | undefined>(undefined);
  const [range, setRange] = useState<(typeof RANGES)[number]>("24h");
  const [data, setData] = useState<Dashboard | null>(null);
  const [hosting, setHosting] = useState<Hosting | null>(null);
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
      const r = await fetch(`/api/v1/operator/earnings?range=${range}`, { cache: "no-store" });
      if (!r.ok) throw new Error((await r.json().catch(() => null))?.error?.message ?? `HTTP ${r.status}`);
      setData(await r.json());
      setErr(null);
      // Hosting money comes from the hosting dashboard's API; the page works without it.
      fetch(`/api/v1/disk?range=${range}`, { cache: "no-store" })
        .then((h) => (h.ok ? h.json() : null))
        .then((h) => setHosting(h && h.by_kind ? h : null))
        .catch(() => setHosting(null));
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
    const t = setInterval(load, 60_000);
    return () => clearInterval(t);
  }, [me, range, load]);

  if (me === undefined) return <p className="text-[var(--color-dim)] text-sm">loading…</p>;
  if (!me) {
    return (
      <div className="space-y-4 text-sm leading-7">
        <p>Sign in to manage your fleet and see what your nodes earned against what they cost to run. Payouts need CoinPay connected.</p>
        <SignInOptions next="/dashboard/nodes" compact />
        <p className="text-[var(--color-dim)]">
          No node yet? <a href="/providers">run one</a>, then come back.
        </p>
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

      {err && <p className="text-[var(--color-warn)]">Could not load your earnings: {err}</p>}

      {data && (
        <>
          <Summary data={data} />
          {hosting && <HostingSummary hosting={hosting} range={data.range} />}
          {data.nodes.length > 0 && <Chart data={data} />}
          <NodesTable data={data} onChange={load} hosting={hosting} />
          <AutoscaleTable />
          {data.by_type.length > 0 && <TypesTable data={data} />}
          <ClaimForm onDone={load} />
          <Notes data={data} />
        </>
      )}
    </div>
  );
}

function Summary({ data }: { data: Dashboard }) {
  const t = data.totals;
  const c0 = data.nodes.filter((n) => n.kind === "c0mpute");
  const tiles: { k: string; v: string; sub: string; warn?: boolean; dim?: boolean }[] = [
    { k: "earned from others", v: usd(t.earned_usd), sub: `${t.paid_jobs.toLocaleString()} paid jobs, settled` },
    {
      k: "spend",
      v: usd(t.spend_usd),
      sub: c0.some((n) => n.spend_basis === "fixed") ? "your hosting costs" : "no hosting cost set",
    },
    { k: "net", v: usd(t.net_usd), sub: t.net_usd >= 0 ? "in profit" : "running at a loss", warn: t.net_usd < 0 },
    { k: "your own jobs", v: usd(t.own_usd), sub: `${t.own_jobs.toLocaleString()} jobs you paid for: not profit`, dim: true },
  ];
  return (
    <section className="grid grid-cols-2 sm:grid-cols-4 gap-3" aria-label="totals">
      {tiles.map((x) => (
        <div key={x.k} className="border border-[var(--color-rule)] bg-[var(--color-card)] p-3 space-y-1">
          <div className="text-xs text-[var(--color-dim)]">{x.k}</div>
          <div className={`text-xl font-bold ${x.warn && x.k !== "spend" ? "text-[var(--color-warn)]" : x.dim ? "text-[var(--color-fg)]" : "accent"}`}>{x.v}</div>
          <div className={`text-xs ${x.warn && x.k === "spend" ? "text-[var(--color-warn)]" : "text-[var(--color-dim)]"}`}>{x.sub}</div>
        </div>
      ))}
      <p className="col-span-2 sm:col-span-4 text-xs text-[var(--color-dim)]">
        {t.jobs.toLocaleString()} jobs completed on c0mpute, {duration(t.busy_s)} busy. Auction prices agreed for them: {usd(t.agreed_usd)}
        {" "}(an offer, not what settles).
      </p>
    </section>
  );
}

/** Earned vs spend per bucket: paired bars on one axis, net as a line. */
function Chart({ data }: { data: Dashboard }) {
  const [hover, setHover] = useState<number | null>(null);
  const W = 720;
  const H = 220;
  const pad = { l: 52, r: 8, t: 10, b: 26 };
  const b = data.buckets;
  const max = useMemo(() => Math.max(1e-9, ...b.map((x) => Math.max(x.earned_usd, x.spend_usd))), [b]);
  const minNet = Math.min(0, ...b.map((x) => x.net_usd));
  const lo = Math.min(0, minNet);
  const y = (v: number) => pad.t + (H - pad.t - pad.b) * (1 - (v - lo) / (max - lo || 1));
  const slot = (W - pad.l - pad.r) / b.length;
  const bw = Math.max(1, Math.min(14, (slot - 4) / 2));
  const x0 = (i: number) => pad.l + i * slot + slot / 2;
  const every = Math.ceil(b.length / 6);
  const net = b.map((x, i) => `${i ? "L" : "M"}${x0(i).toFixed(1)},${y(x.net_usd).toFixed(1)}`).join(" ");
  const h = hover !== null ? b[hover] : null;
  const empty = data.totals.earned_usd === 0 && data.totals.spend_usd === 0;

  return (
    <section className="space-y-2" aria-label="earned vs spend over time">
      <div className="flex flex-wrap items-center gap-4 text-xs text-[var(--color-dim)]">
        <span className="flex items-center gap-1">
          <span className="inline-block w-3 h-3 rounded-sm" style={{ background: EARNED }} /> earned from others
        </span>
        <span className="flex items-center gap-1">
          <span className="inline-block w-3 h-3 rounded-sm" style={{ background: SPEND }} /> spend
        </span>
        <span className="flex items-center gap-1">
          <span className="inline-block w-4 h-0.5 bg-[var(--color-fg)]" /> net
        </span>
        <span className="ml-auto">per {duration(data.bucket_ms / 1000)}</span>
      </div>
      <div className="relative overflow-x-auto">
        <svg viewBox={`0 0 ${W} ${H}`} className="w-full h-auto min-w-[560px]" role="img" aria-label="earned and spend per time bucket" onMouseLeave={() => setHover(null)}>
          {[lo, (lo + max) / 2, max].map((v, i) => (
            <g key={i}>
              <line x1={pad.l} x2={W - pad.r} y1={y(v)} y2={y(v)} stroke="var(--color-rule)" strokeWidth={1} />
              <text x={pad.l - 6} y={y(v) + 4} textAnchor="end" fontSize={10} fill="var(--color-dim)">
                {usd(v)}
              </text>
            </g>
          ))}
          {lo < 0 && <line x1={pad.l} x2={W - pad.r} y1={y(0)} y2={y(0)} stroke="var(--color-muted)" strokeWidth={1} />}
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
              <rect
                x={pad.l + i * slot}
                y={pad.t}
                width={slot}
                height={H - pad.t - pad.b}
                fill="transparent"
                onMouseEnter={() => setHover(i)}
                onTouchStart={() => setHover(i)}
              />
            </g>
          ))}
          <path d={net} fill="none" stroke="var(--color-fg)" strokeWidth={2} pointerEvents="none" />
        </svg>
        {h && (
          <div className="absolute top-1 right-1 border border-[var(--color-rule)] bg-[var(--color-panel)] px-3 py-2 text-xs space-y-0.5 pointer-events-none">
            <div className="text-[var(--color-dim)]">{new Date(h.t).toLocaleString()}</div>
            <div>earned from others {usd(h.earned_usd)} · {h.paid_jobs} jobs</div>
            {h.own_usd > 0 && <div className="text-[var(--color-dim)]">your own jobs {usd(h.own_usd)} · {h.own_jobs}</div>}
            <div>spend {usd(h.spend_usd)}</div>
            <div className={h.net_usd < 0 ? "text-[var(--color-warn)]" : "accent"}>net {usd(h.net_usd)}</div>
          </div>
        )}
        {empty && (
          <p className="absolute inset-0 flex items-center justify-center text-xs text-[var(--color-dim)] pointer-events-none">
            no settled pay and no costs in this window
          </p>
        )}
      </div>
      <details className="text-xs">
        <summary className="cursor-pointer text-[var(--color-dim)]">table view</summary>
        <div className="overflow-x-auto">
          <table className="w-full mt-2">
            <thead className="text-[var(--color-dim)]">
              <tr>
                <th className="text-left font-normal py-1">from</th>
                <th className="text-right font-normal">paid jobs</th>
                <th className="text-right font-normal">earned from others</th>
                <th className="text-right font-normal">your own jobs</th>
                <th className="text-right font-normal">spend</th>
                <th className="text-right font-normal">net</th>
              </tr>
            </thead>
            <tbody>
              {b.map((x) => (
                <tr key={x.t} className="border-t border-[var(--color-rule)]">
                  <td className="py-1">{new Date(x.t).toLocaleString()}</td>
                  <td className="text-right">{x.paid_jobs}</td>
                  <td className="text-right">{usd(x.earned_usd)}</td>
                  <td className="text-right">{usd(x.own_usd)}</td>
                  <td className="text-right">{usd(x.spend_usd)}</td>
                  <td className="text-right">{usd(x.net_usd)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </details>
    </section>
  );
}

function bytesShort(n: number): string {
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(0)} MiB`;
  if (n < 1024 ** 4) return `${(n / 1024 ** 3).toFixed(1)} GiB`;
  return `${(n / 1024 ** 4).toFixed(2)} TiB`;
}

/** Hosting next to compute: disk + app earnings, and what your sites and apps cost. */
function HostingSummary({ hosting, range }: { hosting: Hosting; range: string }) {
  const k = hosting.by_kind;
  const cpu = hosting.nodes.reduce((a, n) => a + n.app_cpu_hours, 0);
  const held = hosting.nodes.reduce((a, n) => a + n.held_bytes, 0);
  const apps = hosting.nodes.reduce((a, n) => a + n.apps, 0);
  const tiles = [
    { k: "disk earned", v: usd(k.storage.earned_usd), sub: `holding ${bytesShort(held)} of others' sites` },
    { k: "apps earned", v: usd(k.apps.earned_usd), sub: `${apps} apps run, ${cpu.toFixed(1)} CPU-h served` },
    { k: "your hosting costs", v: usd(hosting.totals.spend_usd), sub: `sites ${usd(k.storage.spend_usd)} · apps ${usd(k.apps.spend_usd)} (CPU, RAM, disk, bandwidth)` },
    {
      k: "hosting net",
      v: usd(hosting.totals.net_usd),
      sub: hosting.totals.own_usd > 0 ? `+${usd(hosting.totals.own_usd)} paid to yourself` : "earned minus costs",
      warn: hosting.totals.net_usd < 0,
    },
  ];
  return (
    <section className="space-y-2" aria-label="hosting">
      <div className="flex items-baseline justify-between gap-2">
        <h2 className="font-bold">hosting: sites, apps, disk</h2>
        <a href={`/dashboard/disk?range=${range}`} className="text-xs">
          details →
        </a>
      </div>
      <div className="grid grid-cols-2 sm:grid-cols-4 gap-3">
        {tiles.map((x) => (
          <div key={x.k} className="border border-[var(--color-rule)] bg-[var(--color-card)] p-3 space-y-1">
            <div className="text-xs text-[var(--color-dim)]">{x.k}</div>
            <div className={`text-xl font-bold ${x.warn ? "text-[var(--color-warn)]" : "accent"}`}>{x.v}</div>
            <div className="text-xs text-[var(--color-dim)]">{x.sub}</div>
          </div>
        ))}
      </div>
      <p className="text-xs text-[var(--color-dim)]">
        Not in the compute totals above. Sites: accrued by the hosting settlement rule. Apps: metered by the node that ran them at its <code>[apps] price_*</code> rates, not paid out yet.
      </p>
    </section>
  );
}

function NodesTable({ data, onChange, hosting }: { data: Dashboard; onChange: () => void; hosting: Hosting | null }) {
  const hostingOf = new Map((hosting?.nodes ?? []).map((n) => [n.node, n]));
  if (data.nodes.length === 0) {
    return (
      <section className="border border-[var(--color-rule)] p-4 space-y-2 leading-7">
        <h2 className="font-bold">no nodes on this account yet</h2>
        <p className="text-[var(--color-dim)]">
          Add a node below by its peer id or the 8-character id on <a href="/nodes">/nodes</a>. Nodes you linked for hosting
          (<code>c0mpute chat connect hosting</code>) show up here by themselves.
        </p>
      </section>
    );
  }
  return (
    <section className="space-y-2">
      <h2 className="font-bold">per node</h2>
      <div className="overflow-x-auto">
        <table className="w-full min-w-[760px] text-xs [&_td]:px-1 [&_th]:px-1">
          <thead className="text-[var(--color-dim)]">
            <tr>
              <th className="text-left font-normal py-1">node</th>
              <th className="text-right font-normal">jobs</th>
              <th className="text-right font-normal">busy</th>
              <th className="text-right font-normal">earned from others</th>
              <th className="text-right font-normal">your own jobs</th>
              <th className="text-right font-normal">auction (unsettled)</th>
              <th className="text-right font-normal">spend</th>
              <th className="text-right font-normal">net</th>
              <th className="text-right font-normal">hosting earned</th>
              <th className="text-left font-normal pl-3">running cost</th>
            </tr>
          </thead>
          <tbody>
            {data.nodes.map((n) => (
              <tr key={n.node} className="border-t border-[var(--color-rule)] align-top">
                <td className="py-2 pr-2">
                  <div className="flex items-center gap-1">
                    <span className={n.online ? "accent" : "text-[var(--color-dim)]"} aria-label={n.online ? "online" : "offline"}>
                      {n.online ? "●" : "○"}
                    </span>
                    <span>{n.label ?? n.node}</span>
                  </div>
                  <div className="text-[var(--color-dim)]">
                    {n.kind === "infernet" ? "Infernet provider" : `${n.node} · ${n.class}`}
                    {n.via === "hosting" ? " · hosting link" : n.via === "key" ? " · claimed with API key" : ""}
                  </div>
                </td>
                <td className="text-right">{n.jobs}</td>
                <td className="text-right">
                  {duration(n.busy_s)}
                  <div className="text-[var(--color-dim)]">{(n.utilization * 100).toFixed(n.utilization < 0.1 ? 1 : 0)}%</div>
                </td>
                <td className="text-right">
                  {usd(n.earned_usd)}
                  <div className="text-[var(--color-dim)]">{n.paid_jobs} jobs</div>
                </td>
                <td className="text-right text-[var(--color-dim)]">
                  {usd(n.own_usd)}
                  <div>{n.own_jobs} jobs</div>
                </td>
                <td className="text-right text-[var(--color-dim)]">{n.kind === "c0mpute" ? usd(n.agreed_usd) : "-"}</td>
                <td className="text-right">
                  {n.spend_basis === "none" ? "-" : usd(n.spend_usd)}
                </td>
                <td className={`text-right ${n.net_usd < 0 ? "text-[var(--color-warn)]" : ""}`}>{usd(n.net_usd)}</td>
                <td className="text-right">
                  {(() => {
                    const h = hostingOf.get(n.node);
                    if (!h) return <span className="text-[var(--color-dim)]">-</span>;
                    return (
                      <>
                        {usd(h.earned_usd + h.app_earned_usd)}
                        <div className="text-xs text-[var(--color-dim)]">
                          disk {usd(h.earned_usd)} · apps {usd(h.app_earned_usd)}
                          {h.app_cpu_hours > 0 ? ` (${h.app_cpu_hours.toFixed(1)} CPU-h)` : ""}
                        </div>
                      </>
                    );
                  })()}
                </td>
                <td className="pl-3">{n.kind === "c0mpute" ? <CostEditor node={n} onSaved={onChange} /> : <span className="text-[var(--color-dim)]">-</span>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function CostEditor({ node, onSaved }: { node: NodeOut; onSaved: () => void }) {
  const [open, setOpen] = useState(false);
  const [amount, setAmount] = useState(node.cost_usd_per_hour != null ? String(node.cost_usd_per_hour) : "");
  const [per, setPer] = useState<"hour" | "month">("hour");
  const [msg, setMsg] = useState<string | null>(null);
  const send = async (method: string, body?: unknown) => {
    const r = await fetch(method === "DELETE" ? `/api/v1/operator/nodes?node=${node.node}` : "/api/v1/operator/nodes", {
      method,
      headers: { "content-type": "application/json" },
      body: body ? JSON.stringify(body) : undefined,
    });
    if (!r.ok) setMsg((await r.json().catch(() => null))?.error?.message ?? `HTTP ${r.status}`);
    else {
      setMsg(null);
      setOpen(false);
      onSaved();
    }
  };
  if (!open) {
    return (
      <button
        className="text-left underline decoration-dotted text-[var(--color-dim)]"
        onClick={() => setOpen(true)}
      >
        {node.cost_usd_per_hour != null ? `$${+node.cost_usd_per_hour.toPrecision(3)}/h · edit` : "add hosting cost"}
      </button>
    );
  }
  return (
    <form
      className="flex flex-wrap items-center gap-1"
      onSubmit={(e) => {
        e.preventDefault();
        send("PATCH", { node: node.node, cost: amount, per });
      }}
    >
      <input
        value={amount}
        onChange={(e) => setAmount(e.target.value)}
        inputMode="decimal"
        placeholder="0.087"
        aria-label="running cost in dollars"
        className="w-20 bg-[var(--color-card)] border border-[var(--color-rule)] px-2 py-1"
      />
      <select value={per} onChange={(e) => setPer(e.target.value as "hour" | "month")} className="bg-[var(--color-card)] border border-[var(--color-rule)] px-1 py-1">
        <option value="hour">/hour</option>
        <option value="month">/month</option>
      </select>
      <button className="border border-[var(--color-accent)] px-2 py-1 accent">save</button>
      <button type="button" className="px-2 py-1 text-[var(--color-dim)]" onClick={() => send("PATCH", { node: node.node, cost: null })}>
        clear cost
      </button>
      {node.via !== "hosting" && (
        <button type="button" className="px-2 py-1 text-[var(--color-dim)]" onClick={() => send("DELETE")}>
          remove node
        </button>
      )}
      {msg && <span className="w-full text-[var(--color-warn)]">{msg}</span>}
    </form>
  );
}

function TypesTable({ data }: { data: Dashboard }) {
  return (
    <section className="space-y-2">
      <h2 className="font-bold">c0mpute jobs by type</h2>
      <div className="overflow-x-auto">
        <table className="w-full min-w-[620px] text-xs [&_td]:px-1 [&_th]:px-1">
          <thead className="text-[var(--color-dim)]">
            <tr>
              <th className="text-left font-normal py-1">type</th>
              <th className="text-right font-normal">jobs</th>
              <th className="text-right font-normal">busy</th>
              <th className="text-right font-normal">avg run</th>
              <th className="text-right font-normal">auction price (unsettled)</th>
            </tr>
          </thead>
          <tbody>
            {data.by_type.map((t) => (
              <tr key={t.type} className="border-t border-[var(--color-rule)]">
                <td className="py-1">{jobLabel(t.type)}</td>
                <td className="text-right">{t.jobs}</td>
                <td className="text-right">{t.busy_s ? duration(t.busy_s) : "-"}</td>
                <td className="text-right">{t.jobs && t.busy_s ? duration(t.busy_s / t.jobs) : "-"}</td>
                <td className="text-right">{usd(t.agreed_usd)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function ClaimForm({ onDone }: { onDone: () => void }) {
  const [v, setV] = useState("");
  const [mode, setMode] = useState<"node" | "nickname">("node");
  const [msg, setMsg] = useState<string | null>(null);
  return (
    <section className="space-y-2">
      <h2 className="font-bold">add a node</h2>
      <form
        className="flex flex-wrap gap-2"
        onSubmit={async (e) => {
          e.preventDefault();
          const r = await fetch("/api/v1/operator/nodes", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(mode === "node" ? { node: v } : { nickname: v }),
          });
          const j = await r.json().catch(() => null);
          if (!r.ok) setMsg(j?.error?.message ?? `HTTP ${r.status}`);
          else {
            setMsg(mode === "nickname" ? `added ${j.nodes.length} node(s)` : j.online ? "added" : "added (not online right now)");
            setV("");
            onDone();
          }
        }}
      >
        <select value={mode} onChange={(e) => setMode(e.target.value as "node" | "nickname")} className="bg-[var(--color-card)] border border-[var(--color-rule)] px-2 py-2">
          <option value="node">peer id / node id</option>
          <option value="nickname">every node named</option>
        </select>
        <input
          value={v}
          onChange={(e) => setV(e.target.value)}
          placeholder={mode === "node" ? "12D3KooW… or ffig7P4J" : "your c0mpute name"}
          className="flex-1 min-w-0 bg-[var(--color-card)] border border-[var(--color-rule)] px-3 py-2"
        />
        <button className="border border-[var(--color-accent)] px-4 py-2 accent">add</button>
      </form>
      {msg && <p className="text-xs text-[var(--color-dim)]">{msg}</p>}
      <p className="text-xs text-[var(--color-dim)] leading-6">
        Your peer id: <code>c0mpute status --json | jq -r .peer_id</code>. Or claim from the node itself with its API key, signed in with{" "}
        <code>c0mpute chat login</code>:
      </p>
      <pre className="text-xs overflow-x-auto bg-[var(--color-card)] p-3">{`curl -sS https://c0mpute.com/api/v1/operator/nodes \\
  -H "authorization: Bearer $(jq -r .key ~/.c0mpute/chat.json)" \\
  -d "{\\"node\\":\\"$(c0mpute status --json | jq -r .peer_id)\\"}"`}</pre>
    </section>
  );
}

function Notes({ data }: { data: Dashboard }) {
  return (
    <section className="text-xs text-[var(--color-dim)] leading-6 space-y-1">
      <p>
        <b>earned from others</b> is settled pay: the credits in the Infernet ledger for your nodes (a c0mpute node that serves chat
        is paid through the provider Infernet opened for it), after the 5% house fee, minus anything you paid yourself. It is
        reported per hour and spread evenly across shorter bars.
      </p>
      <p>
        <b>your own jobs</b> are settled credits for jobs your own account paid for (for the platform, any admin account): money that
        went round in a circle, so it is not profit and not in net. <b>auction (unsettled)</b> is the price agreed in the c0mpute
        auction, as the status aggregators heard it: an offer, not a payment.
      </p>
      <p>
        <b>spend</b> is the hosting cost you set for a node (for example $0.087/hour for a rented GPU, power included), accrued for
        every hour of the window busy or not. A node with no cost set counts $0.
      </p>
      {data.history_from != null && <p>c0mpute job history on file since {new Date(data.history_from).toLocaleString()}.</p>}
      {data.infernet === null && <p className="text-[var(--color-warn)]">Settled pay could not be read from Infernet right now, so earned shows $0.</p>}
      {data.infernet && !data.infernet.linked && <p>No Infernet account is linked to this CoinPay sign-in yet.</p>}
    </section>
  );
}

// Mirrors lib/autoscale.ts AutoscaleNodeOut.
type AutoscaleNode = {
  node: string;
  provider: string;
  instance: string;
  state: string;
  reason: string;
  last_transition_ms: number;
  reported_ms: number;
  saved_usd: number;
  running_usd_per_hour: number | null;
  stopped_usd_per_hour: number | null;
  idle_minutes: number | null;
  models: string[];
  events: { state: string; reason: string; at_ms: number }[];
};

function ago(ms: number): string {
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 172_800) return `${(s / 3600).toFixed(1)} h ago`;
  return `${Math.round(s / 86_400)} days ago`;
}

function stateClass(state: string): string {
  if (state === "running") return "accent";
  if (state === "error") return "text-[var(--color-warn)]";
  return "text-[var(--color-dim)]";
}

/** Rented nodes `c0mpute autoscale` stops when idle and starts on demand (docs#autoscale). */
function AutoscaleTable() {
  const [nodes, setNodes] = useState<AutoscaleNode[] | null>(null);
  useEffect(() => {
    let live = true;
    const get = () =>
      fetch("/api/v1/operator/autoscale", { cache: "no-store" })
        .then((r) => (r.ok ? r.json() : null))
        .then((d) => live && setNodes(Array.isArray(d?.nodes) ? d.nodes : []))
        .catch(() => live && setNodes([]));
    get();
    const t = setInterval(get, 60_000);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, []);
  if (nodes === null) return null;
  if (nodes.length === 0) {
    return (
      <p className="text-xs text-[var(--color-dim)]">
        Renting a GPU node? <a href="/docs#autoscale">c0mpute autoscale</a> stops it when the network has no work for it and starts it when jobs wait.
      </p>
    );
  }
  const saved = nodes.reduce((a, n) => a + n.saved_usd, 0);
  return (
    <section className="space-y-2" aria-label="autoscale">
      <h2 className="font-bold">
        autoscale <span className="font-normal text-xs text-[var(--color-dim)]">saved {usd(saved)} so far · <a href="/docs#autoscale">how it works</a></span>
      </h2>
      <div className="overflow-x-auto">
        <table className="w-full min-w-[640px] text-xs [&_td]:px-1 [&_th]:px-1">
          <thead className="text-[var(--color-dim)]">
            <tr>
              <th className="text-left font-normal py-1">node</th>
              <th className="text-left font-normal">state</th>
              <th className="text-left font-normal">since</th>
              <th className="text-left font-normal">why</th>
              <th className="text-right font-normal">saved</th>
            </tr>
          </thead>
          <tbody>
            {nodes.map((n) => (
              <tr key={n.node} className="border-t border-[var(--color-rule)] align-top">
                <td className="py-2 pr-2">
                  <div>{n.node}</div>
                  <div className="text-[var(--color-dim)]">
                    {n.provider} {n.instance}
                    {n.running_usd_per_hour != null && ` · ${usd(n.running_usd_per_hour)}/h running`}
                    {n.stopped_usd_per_hour != null && `, ${usd(n.stopped_usd_per_hour)}/h stopped`}
                  </div>
                </td>
                <td className={stateClass(n.state)}>
                  <span className="border border-current px-1">{n.state}</span>
                </td>
                <td title={new Date(n.last_transition_ms).toLocaleString()}>
                  {ago(n.last_transition_ms)}
                  {Date.now() - n.reported_ms > 10 * 60_000 && <div className="text-[var(--color-warn)]">no report for {ago(n.reported_ms).replace(" ago", "")}</div>}
                </td>
                <td className="max-w-[260px]">
                  <div>{n.reason || "-"}</div>
                  {n.events.length > 1 && (
                    <details>
                      <summary className="cursor-pointer text-[var(--color-dim)]">last {n.events.length} changes</summary>
                      <ul className="mt-1 space-y-0.5 text-[var(--color-dim)]">
                        {n.events.map((e, i) => (
                          <li key={i}>
                            {new Date(e.at_ms).toLocaleString()}: {e.state}
                            {e.reason ? `, ${e.reason}` : ""}
                          </li>
                        ))}
                      </ul>
                    </details>
                  )}
                </td>
                <td className="text-right">{usd(n.saved_usd)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}
