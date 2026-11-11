/**
 * Node: this machine's worker. Version and update, whether the worker runs
 * and how, every role it was started with and whether that role is serving
 * right now or gated (and why), the tags it advertises, the jobs running on
 * it, recent job history, the log, and the bench score.
 */
import type { Container, Theme } from "@profullstack/hqtui";
import { humanRate, summarizeWorkloads } from "../bench.ts";
import { compareVersions, type NodeSnapshot } from "../node.ts";
import { ago } from "../network.ts";
import type { AppState } from "../state.ts";
import { clock, drawLines, duration, panel, wrapBlock, type Line } from "./common.ts";

export interface NodeHandlers {
  onScrollRoles?: (delta: number) => void;
}

const GIB = 2 ** 30;

/** One line per fact about the node (pure; tested). */
export function summaryRows(node: NodeSnapshot, state: AppState, theme: Theme): { label: string; value: string; color?: ReturnType<() => Theme["foreground"]> }[] {
  const rows: { label: string; value: string; color?: Theme["foreground"] }[] = [];
  const v = node.cliVersion ?? node.status?.version ?? null;
  if (!v) {
    rows.push({ label: "c0mpute", value: node.statusError ?? "checking…", color: theme.muted });
  } else if (node.latest && compareVersions(v, node.latest) < 0) {
    rows.push({ label: "c0mpute", value: `${v} · ${node.latest} available: c0mpute update`, color: theme.warning });
  } else {
    rows.push({ label: "c0mpute", value: `${v}${node.latest ? " · up to date" : ""}${node.status ? ` · auto-update ${node.status.auto_update ? "on" : "off"}` : ""}`, color: theme.success });
  }
  const w = node.worker;
  if (w.state === "running") {
    rows.push({
      label: "worker",
      value: `running · pid ${w.pid} · ${w.via ?? "process"}${w.otherUser ? " (another user)" : ""}${w.ageS !== null ? ` · up ${duration(w.ageS)}` : ""}`,
      color: theme.success,
    });
  } else if (w.state === "stale") {
    rows.push({ label: "worker", value: `not running (stale pid ${w.pid}): c0mpute worker start -d`, color: theme.warning });
  } else {
    rows.push({ label: "worker", value: "not running: c0mpute worker start -d", color: theme.warning });
  }
  const s = node.status;
  if (s) {
    const id = s.peer_id ? `…${s.peer_id.slice(-8)}` : "no identity yet";
    rows.push({ label: "node", value: `${s.name ?? "(no nickname: c0mpute name <handle>)"} · ${id}${s.private ? " · PRIVATE (not advertised)" : ""}` });
    const h = s.host;
    rows.push({
      label: "host",
      value: `load ${h.load_1m.toFixed(1)}/${h.cpus} CPUs · RAM ${(h.ram_free_bytes / GIB).toFixed(1)} of ${(h.ram_total_bytes / GIB).toFixed(0)} GiB free · ${h.job_threads} threads/job · ${s.hardware.replace("c0mpute:", "")}`,
    });
  }
  const run = node.running;
  rows.push({
    label: "running",
    value: run.length ? run.map((r) => `${r.what} ${duration(r.ageS)}`).join(" · ") : w.state === "running" ? "idle (no job processes)" : "-",
    color: run.length ? theme.warning : theme.muted,
  });
  if (node.jobTimes.length) {
    rows.push({
      label: "recent",
      value: node.jobTimes
        .slice(0, 3)
        .map((j) => `${j.workload} ${ago((state.now - j.lastAt) / 1000)} ago (avg ${duration(j.avgMs / 1000)})`)
        .join(" · "),
      color: theme.muted,
    });
  }
  const me = state.me.data;
  if (me?.balance) {
    rows.push({ label: "earned", value: `$${Number(me.balance.earned_spendable_usd).toFixed(4)} spendable from your nodes · balance $${Number(me.balance.spendable_usd).toFixed(2)}`, color: theme.accent });
  }
  const b = state.bench.bench;
  rows.push({ label: "bench", value: b ? `score ${b.score.toLocaleString("en-US")}${b.metadata.quick ? " (quick run, not advertised)" : ""} · b shows it` : "no score: c0mpute bench", color: b ? theme.accent : theme.muted });
  return rows;
}

/** The roles panel as lines: role, state, detail or the reason it is gated (pure; tested). */
export function roleLines(node: NodeSnapshot, width: number, theme: Theme): Line[] {
  const out: Line[] = [];
  const s = node.status;
  if (!s) {
    if (node.statusError) for (const l of wrapBlock(node.statusError, width, "")) out.push({ text: l, fg: theme.warning });
    if (node.statusText) {
      out.push({ text: "" });
      for (const t of node.statusText) for (const l of wrapBlock(t, width, "")) out.push({ text: l, fg: theme.foreground });
    }
    if (!node.statusError && !node.statusText) out.push({ text: "asking c0mpute status --json…", fg: theme.muted });
    return out;
  }
  const serving = s.role_states.filter((r) => r.serving).length;
  out.push({ text: `${serving}/${s.role_states.length} roles serving · roles from ${s.roles_source}`, fg: theme.muted });
  if (node.worker.otherUser) {
    out.push({ text: "worker runs as another user: gates below use your config/env, its own may differ", fg: theme.warning });
  }
  const nameW = Math.min(14, Math.max(...s.role_states.map((r) => r.role.length)) + 1);
  for (const r of s.role_states) {
    const mark = r.serving ? "✓" : "✗";
    const text = r.serving ? (r.detail ?? "serving") : (r.reason ?? "not serving");
    const lines = wrapBlock(text, width - nameW - 3, "");
    lines.forEach((l, i) =>
      out.push({
        text: i === 0 ? `${mark} ${r.role.padEnd(nameW)}${l}` : `${" ".repeat(nameW + 2)}${l}`,
        fg: r.serving ? (i === 0 ? theme.success : theme.muted) : i === 0 ? theme.warning : theme.muted,
      }),
    );
  }
  out.push({ text: "" });
  const tags = s.private ? "none (private mode)" : s.tags.join(" ");
  for (const l of wrapBlock(`advertised: ${tags}`, width, "")) out.push({ text: l, fg: theme.muted });
  if (node.statusAt) out.push({ text: `checked ${clock(new Date(node.statusAt))} · r re-checks`, fg: theme.muted, dim: true });
  return out;
}

function benchPanel(parent: Container, state: AppState, theme: Theme): void {
  const report = state.bench.bench;
  panel(
    parent,
    theme,
    "Bench",
    {
      footer: report ? `${report.metadata.cpu} · ${report.metadata.cores} cores · ${report.metadata.start_time}${report.metadata.quick ? " · quick run, not advertised" : ""}` : undefined,
      subtitle: "b: roles",
    },
    (p) => {
      if (!report) {
        p.label(state.bench.benchError ?? "no benchmark yet", { fg: state.bench.benchError ? theme.danger : theme.muted });
        p.label("run `c0mpute bench` to measure this node and publish a score", { fg: theme.muted });
        p.label(state.bench.benchPath, { fg: theme.muted });
        return;
      }
      p.row({ size: 1 }, (row) => {
        row.text(`score ${report.score.toLocaleString("en-US")}`, { fg: theme.accent, bold: true, size: 20 });
        row.text("1000 = one reference core on every workload", { fg: theme.muted, size: "fill" });
      });
      for (const s of summarizeWorkloads(report)) {
        p.row({ size: 1 }, (row) => {
          row.text(`${s.name} (${s.params})`, { fg: theme.foreground, bold: true, size: 20 });
          row.text(`${humanRate(s.name, s.best.result.throughput)} @ ${s.best.threads} thr`, { fg: theme.foreground, size: "fill" });
        });
        p.meter({
          value: Math.round(s.efficiency * 100),
          max: 100,
          label: "  scaling",
          text: `${Math.round(s.efficiency * 100)}% of ideal`,
          color: s.efficiency >= 0.7 ? theme.success : s.efficiency >= 0.4 ? theme.warning : theme.danger,
          labelWidth: 11,
          size: 1,
        });
        p.sparkline({ values: s.speedups, color: theme.accent, label: "  speedup", size: 1 });
      }
    },
  );
}

function logPanel(parent: Container, node: NodeSnapshot, theme: Theme, size: number | string): void {
  panel(parent, theme, "Log", { size, subtitle: node.logSource ? node.logSource.slice(-50) : undefined }, (p) => {
    p.draw((surface) => {
      const lines: Line[] = [];
      for (const l of node.logs) {
        const text = `${l.time ? `${l.time} ` : ""}${l.level === "INFO" ? "" : `${l.level} `}${l.message}`;
        lines.push({ text, fg: l.level === "ERROR" ? theme.danger : l.level === "WARN" ? theme.warning : theme.muted });
      }
      if (!lines.length) lines.push({ text: node.logSource || "reading the log…", fg: theme.muted });
      drawLines(surface, lines, 0, theme, true);
    });
  });
}

export function renderNode(ui: Container, state: AppState, theme: Theme, h: NodeHandlers = {}): void {
  const node = state.node;
  const all = summaryRows(node, state, theme);
  const wide = ui.width >= 120;
  // Short terminals keep the essentials so the roles still get the room.
  const rows = wide || ui.height >= 34 ? all : all.filter((r) => ["c0mpute", "worker", "host", "running", "bench"].includes(r.label));
  const summary = (parent: Container, size: number | string) =>
    panel(parent, theme, "This node", { size, subtitle: node.cliPath !== "c0mpute" ? node.cliPath : undefined }, (p) => {
      p.keyValues(
        rows.map((r) => ({ label: r.label, value: r.value, color: r.color })),
        { labelWidth: 8, spread: false, size: "fill" },
      );
    });
  const roles = (parent: Container) => {
    if (state.nodePanel === "bench") return benchPanel(parent, state, theme);
    panel(parent, theme, "Roles", { subtitle: "b: bench" }, (p) => {
      p.draw((surface) => {
        drawLines(surface, roleLines(node, surface.width - 1, theme), state.nodeScroll, theme, false);
        p.ctx.hit({ rect: surface.hitRect(), onScroll: (d) => h.onScrollRoles?.(d * 2) });
      });
    });
  };
  if (wide) {
    ui.column({ size: "fill" }, (col) => {
      col.row({ size: "fill", gap: 1 }, (row) => {
        row.column({ size: "50%" }, (left) => summary(left, "fill"));
        row.column({ size: "fill" }, (right) => roles(right));
      });
      logPanel(col, node, theme, Math.max(6, Math.min(14, Math.floor(ui.height / 3))));
    });
  } else {
    ui.column({ size: "fill" }, (col) => {
      summary(col, Math.min(rows.length + 2, 10));
      roles(col);
      logPanel(col, node, theme, ui.height >= 34 ? 10 : 5);
    });
  }
}
