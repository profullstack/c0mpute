"use client";

import { useCallback, useEffect, useMemo, useState } from "react";

type Node = {
  node: string;
  peer_id: string | null;
  label: string | null;
  tags: string[];
  verified: boolean;
  online: boolean;
  last_seen_s: number | null;
  version: string | null;
  class: string | null;
  cores: number | null;
  ram_bytes: number | null;
  storage: { free_bytes: number; total_bytes: number } | null;
  jobs_24h: number;
  earned_usd: { h24: number; d30: number; all: number };
  paused: boolean | null;
  pending_commands: number;
  node_state: {
    settings: { models: string[] | null; house_models: boolean | null; uncensored: boolean | null; hosting: boolean | null; storage: boolean | null };
    effective: { served_models: string[]; house_models: boolean | null; uncensored: boolean | null; hosting: boolean | null; storage: boolean | null };
    disk: { free_bytes: number; total_bytes: number } | null;
    update_error: { version: string; error: string } | null;
  } | null;
  state_at: string | null;
};
type Fleet = { nodes: Node[]; totals: { nodes: number; online: number; verified: number; jobs_24h: number; earned_usd: { h24: number; d30: number; all: number } }; payouts_ready: boolean };
type Audit = { log: { id: string; node: string | null; action: string; actor: string; outcome: string; at: string }[] };
type SortKey = "node" | "online" | "version" | "jobs_24h" | "h24" | "d30" | "all";

const usd = (n: number) => `$${n.toFixed(4)}`;
const gb = (b: number | null | undefined) => (b ? `${Math.round(b / 1e9)}G` : "–");
const ago = (s: number | null) => (s == null ? "–" : s < 90 ? `${s}s` : s < 5400 ? `${Math.round(s / 60)}m` : `${Math.round(s / 3600)}h`);
const btn = "border border-[var(--color-rule)] px-2.5 py-1 hover:text-[var(--color-accent)] hover:border-[var(--color-accent)] disabled:opacity-40";
const NODE_SIDE = ["pause", "resume", "update", "restart", "models", "house-models", "uncensored", "hosting", "storage"];
const TOGGLES: { action: string; label: string; restarts?: boolean }[] = [
  { action: "house-models", label: "house models" },
  { action: "uncensored", label: "uncensored" },
  { action: "hosting", label: "hosting", restarts: true },
  { action: "storage", label: "storage", restarts: true },
];
const onoff = (v: boolean | null | undefined) => (v === true ? "on" : v === false ? "off" : "–");

/**
 * Fleet view for the signed-in operator: every node, status, hardware, jobs,
 * earnings, tags; filter, sort, select, and act on one or many nodes. Signed
 * out it renders nothing (the earnings dashboard below shows the sign-in).
 */
export function FleetPanel() {
  const [fleet, setFleet] = useState<Fleet | null | undefined>(undefined);
  const [sel, setSel] = useState<Set<string>>(new Set());
  const [q, setQ] = useState("");
  const [tag, setTag] = useState("");
  const [sort, setSort] = useState<{ k: SortKey; desc: boolean }>({ k: "online", desc: true });
  const [msg, setMsg] = useState<string | null>(null);
  const [audit, setAudit] = useState<Audit | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    const r = await fetch("/api/v1/fleet");
    if (r.status === 401) return setFleet(null);
    if (r.ok) setFleet(await r.json());
  }, []);
  useEffect(() => {
    load();
    const t = setInterval(load, 30_000);
    return () => clearInterval(t);
  }, [load]);

  const tags = useMemo(() => Array.from(new Set((fleet?.nodes ?? []).flatMap((n) => n.tags))).sort(), [fleet]);
  const rows = useMemo(() => {
    const list = (fleet?.nodes ?? []).filter(
      (n) => (!tag || n.tags.includes(tag)) && (!q || `${n.node} ${n.label ?? ""} ${n.tags.join(" ")} ${n.version ?? ""}`.toLowerCase().includes(q.toLowerCase())),
    );
    const v = (n: Node): number | string => {
      switch (sort.k) {
        case "node":
          return (n.label ?? n.node).toLowerCase();
        case "online":
          return n.online ? 1 : 0;
        case "version":
          return n.version ?? "";
        case "jobs_24h":
          return n.jobs_24h;
        default:
          return n.earned_usd[sort.k];
      }
    };
    return list.sort((a, b) => {
      const x = v(a);
      const y = v(b);
      return (x < y ? -1 : x > y ? 1 : 0) * (sort.desc ? -1 : 1);
    });
  }, [fleet, q, tag, sort]);

  if (fleet === undefined || fleet === null) return null;

  const act = async (action: string, args: Record<string, unknown> = {}) => {
    if (!sel.size) return;
    if ((action === "unlink" || action === "restart" || action === "update") && !confirm(`${action} ${sel.size} node(s)?`)) return;
    if (TOGGLES.some((t) => t.action === action)) {
      const t = TOGGLES.find((x) => x.action === action)!;
      const v = args.on === true ? "ON" : args.on === false ? "OFF" : "back to the node's own config";
      if (!confirm(`Turn ${t.label} ${v} on ${sel.size} node(s)?${t.restarts ? " Each node restarts after its running jobs finish." : ""}`)) return;
    }
    if (action === "models") {
      const list = (args.models as string[]) ?? [];
      if (!confirm(list.length ? `Serve only ${list.join(", ")} on ${sel.size} node(s)?` : `Serve every model each of ${sel.size} node(s) has?`)) return;
    }
    setBusy(true);
    setMsg(null);
    try {
      const r = await fetch("/api/v1/fleet/actions", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ action, nodes: Array.from(sel), args }) });
      const j = await r.json().catch(() => ({}));
      if (j.error) setMsg(j.error.message);
      else setMsg(`${action}: ${j.ok} ok${j.failed ? `, ${j.failed} refused (${j.results.filter((x: { ok: boolean }) => !x.ok).map((x: { node: string; detail: string }) => `${x.node}: ${x.detail}`).join("; ")})` : ""}${NODE_SIDE.includes(action) ? " · runs on each node's next poll (about a minute); the node reports its new state here" : ""}`);
      await load();
    } finally {
      setBusy(false);
    }
  };
  const sortBy = (k: SortKey) => setSort((s) => ({ k, desc: s.k === k ? !s.desc : true }));
  const th = (k: SortKey, label: string) => (
    <th className="text-left font-normal px-2 py-1.5 cursor-pointer select-none whitespace-nowrap" onClick={() => sortBy(k)} aria-sort={sort.k === k ? (sort.desc ? "descending" : "ascending") : "none"}>
      {label}
      {sort.k === k ? (sort.desc ? " ▾" : " ▴") : ""}
    </th>
  );
  const allSel = rows.length > 0 && rows.every((n) => sel.has(n.node));

  return (
    <section className="space-y-3 text-sm" aria-labelledby="fleet-h">
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <h2 id="fleet-h" className="font-bold text-lg">
          fleet
        </h2>
        <p className="text-[var(--color-dim)]">
          {fleet.totals.nodes} nodes · {fleet.totals.online} online · {fleet.totals.verified} verified · {fleet.totals.jobs_24h} jobs 24h · earned 24h {usd(fleet.totals.earned_usd.h24)} · 30d{" "}
          {usd(fleet.totals.earned_usd.d30)} · all {usd(fleet.totals.earned_usd.all)}
        </p>
      </div>
      {!fleet.payouts_ready && (
        <p className="border border-[var(--color-warn)] p-2">
          <b>Connect CoinPay to get paid.</b> Earnings and payouts for the whole fleet go to your CoinPay wallet.{" "}
          <a className="underline accent" href="/api/auth/coinpay/login?link=1&next=/dashboard/nodes">
            Connect CoinPay
          </a>
        </p>
      )}
      {fleet.nodes.length === 0 ? (
        <div className="border border-[var(--color-rule)] p-4 space-y-2">
          <p>No nodes on this account yet. On each node:</p>
          <pre className="text-xs overflow-x-auto">{`c0mpute login && c0mpute node link\n# many boxes: create an enrollment token on /account, then\ncurl -fsSL https://c0mpute.com/install.sh | sh -s -- --enroll c0e_...`}</pre>
        </div>
      ) : (
        <>
          <div className="flex flex-wrap gap-2 items-center">
            <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="filter" className="bg-[var(--color-card)] border border-[var(--color-rule)] px-2 py-1 w-40" />
            <select value={tag} onChange={(e) => setTag(e.target.value)} className="bg-[var(--color-card)] border border-[var(--color-rule)] px-2 py-1">
              <option value="">all tags</option>
              {tags.map((t) => (
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </select>
            <span className="text-[var(--color-dim)]">{sel.size} selected:</span>
            {["pause", "resume", "update", "restart"].map((a) => (
              <button key={a} className={btn} disabled={!sel.size || busy} onClick={() => act(a)}>
                {a}
              </button>
            ))}
            <button
              className={btn}
              disabled={!sel.size || busy}
              onClick={() => {
                const t = prompt("Add tags (comma separated)");
                if (t) act("tag", { tags: t });
              }}
            >
              tag
            </button>
            <button
              className={btn}
              disabled={!sel.size || busy}
              onClick={() => {
                const t = prompt("Remove tags (comma separated)");
                if (t) act("untag", { tags: t });
              }}
            >
              untag
            </button>
            <button
              className={btn}
              disabled={sel.size !== 1 || busy}
              onClick={() => {
                const l = prompt("New name");
                if (l !== null) act("rename", { label: l });
              }}
            >
              rename
            </button>
            <button className={btn} disabled={!sel.size || busy} onClick={() => act("unlink")}>
              unlink
            </button>
          </div>
          <div className="flex flex-wrap gap-2 items-center">
            <span className="text-[var(--color-dim)]">node settings:</span>
            <button
              className={btn}
              disabled={!sel.size || busy}
              onClick={() => {
                const first = fleet.nodes.find((n) => sel.has(n.node));
                const cur = first?.node_state?.settings.models?.join(", ") ?? "";
                const t = prompt("Models to serve, comma separated (empty: every model the node has)", cur);
                if (t !== null) act("models", { models: t.split(",").map((m) => m.trim()).filter(Boolean) });
              }}
            >
              models
            </button>
            {TOGGLES.map((t) => (
              <span key={t.action} className="inline-flex">
                <button className={btn} disabled={!sel.size || busy} onClick={() => act(t.action, { on: true })} title={`${t.label} on`}>
                  {t.label} on
                </button>
                <button className={btn} disabled={!sel.size || busy} onClick={() => act(t.action, { on: false })} title={`${t.label} off`}>
                  off
                </button>
              </span>
            ))}
          </div>
          {msg && <p className="text-[var(--color-dim)]">{msg}</p>}
          <div className="overflow-x-auto border border-[var(--color-rule)] rounded">
            <table className="w-full text-xs">
              <thead className="bg-[var(--color-panel)] text-[var(--color-dim)]">
                <tr>
                  <th className="px-2">
                    <input type="checkbox" aria-label="select all" checked={allSel} onChange={() => setSel(allSel ? new Set() : new Set(rows.map((n) => n.node)))} />
                  </th>
                  {th("node", "node")}
                  {th("online", "status")}
                  {th("version", "version")}
                  <th className="text-left font-normal px-2">hardware</th>
                  {th("jobs_24h", "jobs 24h")}
                  {th("h24", "24h")}
                  {th("d30", "30d")}
                  {th("all", "all")}
                  <th className="text-left font-normal px-2">tags</th>
                  <th className="text-left font-normal px-2">settings</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((n) => (
                  <tr key={n.node} className="border-t border-[var(--color-line)]">
                    <td className="px-2">
                      <input
                        type="checkbox"
                        aria-label={`select ${n.node}`}
                        checked={sel.has(n.node)}
                        onChange={() => {
                          const s = new Set(sel);
                          if (s.has(n.node)) s.delete(n.node);
                          else s.add(n.node);
                          setSel(s);
                        }}
                      />
                    </td>
                    <td className="px-2 py-1.5 whitespace-nowrap">
                      <code>{n.node}</code> {n.label && <span className="text-[var(--color-fg)]">{n.label}</span>}
                      {!n.verified && (
                        <span className="ml-1 text-[var(--color-warn)]" title="Claimed, not proven: run `c0mpute node link` on it to manage it">
                          unverified
                        </span>
                      )}
                    </td>
                    <td className="px-2 whitespace-nowrap">
                      <span className={n.online ? "accent" : "text-[var(--color-dim)]"}>{n.online ? "● online" : "○ offline"}</span>
                      <span className="text-[var(--color-dim)]"> {ago(n.last_seen_s)}</span>
                      {n.paused && <span className="ml-1 text-[var(--color-warn)]">paused</span>}
                      {n.pending_commands > 0 && <span className="ml-1 text-[var(--color-dim)]">({n.pending_commands} queued)</span>}
                    </td>
                    <td className="px-2">{n.version ?? "–"}</td>
                    <td className="px-2 whitespace-nowrap text-[var(--color-dim)]">
                      {n.class ?? "?"} · {n.cores ?? "?"}c · {gb(n.ram_bytes)} RAM{n.storage ? ` · ${gb(n.storage.free_bytes)}/${gb(n.storage.total_bytes)} disk` : ""}
                    </td>
                    <td className="px-2">{n.jobs_24h}</td>
                    <td className="px-2">{usd(n.earned_usd.h24)}</td>
                    <td className="px-2">{usd(n.earned_usd.d30)}</td>
                    <td className="px-2">{usd(n.earned_usd.all)}</td>
                    <td className="px-2">{n.tags.join(", ")}</td>
                    <td className="px-2 whitespace-nowrap text-[var(--color-dim)]">
                      {n.node_state ? (
                        <span title={`models served: ${n.node_state.effective.served_models.join(", ") || "none"}${n.state_at ? ` · reported ${n.state_at.slice(0, 16).replace("T", " ")}` : ""}`}>
                          {n.node_state.effective.served_models.length} models{n.node_state.settings.models ? " (allowlist)" : ""} · house {onoff(n.node_state.effective.house_models)} · uncensored{" "}
                          {onoff(n.node_state.effective.uncensored)} · hosting {onoff(n.node_state.effective.hosting)} · storage {onoff(n.node_state.effective.storage)}
                          {n.node_state.update_error && <span className="ml-1 text-[var(--color-warn)]">update failed</span>}
                        </span>
                      ) : (
                        "–"
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <p className="text-xs text-[var(--color-dim)]">
            Earnings are buyer-agreed amounts before the 5% fee. Pause stops a node bidding on new jobs (running jobs finish); update installs the latest release; restart re-executes the worker. Node settings are signed commands the node applies to its own fleet-settings.json: models and the house/uncensored switches take effect at once, hosting and storage after the worker restarts (it waits for running jobs). Alerts for these nodes: <a className="underline" href="/account#alerts">account</a>.{" "}
            <button
              className="underline"
              onClick={async () => {
                const r = await fetch("/api/v1/fleet/audit?limit=50");
                if (r.ok) setAudit(await r.json());
              }}
            >
              audit log
            </button>
          </p>
          {audit && (
            <ul className="text-xs divide-y divide-[var(--color-line)] border border-[var(--color-rule)] rounded max-h-72 overflow-y-auto">
              {audit.log.map((e) => (
                <li key={e.id} className="px-2 py-1 flex flex-wrap gap-x-3">
                  <span className="text-[var(--color-dim)]">{e.at.slice(0, 19).replace("T", " ")}</span>
                  <code>{e.node ?? "account"}</code>
                  <span>{e.action}</span>
                  <span className="text-[var(--color-dim)]">by {e.actor}</span>
                  <span>{e.outcome}</span>
                </li>
              ))}
            </ul>
          )}
        </>
      )}
    </section>
  );
}
