/**
 * The dashboard as a pure function of state. Rendered by the app every frame
 * and by tests through `renderToText`.
 */
import type { Container, Theme } from "@profullstack/hqtui";
import { humanRate, summarizeWorkloads, type State } from "./state.ts";

function clock(d: Date): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

function header(ui: Container, state: State, theme: Theme): void {
  const worker = state.worker;
  const badge =
    worker.kind === "running"
      ? { glyph: "●", label: `WORKER pid ${worker.pid}`, color: theme.success }
      : worker.kind === "stale"
        ? { glyph: "○", label: `WORKER stale pid ${worker.pid}`, color: theme.warning }
        : { glyph: "○", label: "WORKER stopped", color: theme.muted };
  ui.box({ size: 3, border: "rounded", borderColor: theme.border, padding: [0, 1] }, (box) => {
    box.row({ size: 1 }, (row) => {
      row.text("⚡ C0MPUTE", { fg: theme.primary, bold: true, size: 12 });
      row.text(`${badge.glyph} ${badge.label}`, { fg: badge.color, bold: true, size: 28 });
      row.text(
        state.bench ? `score ${state.bench.score.toLocaleString()}` : "no bench score",
        { fg: state.bench ? theme.accent : theme.muted, size: "fill" },
      );
      row.text(`refreshed ${clock(state.loadedAt)}`, { fg: theme.muted, size: 20, align: "right" });
    });
  });
}

function benchPanel(parent: Container, state: State, theme: Theme): void {
  const report = state.bench;
  parent.panel(
    {
      title: " BENCH ",
      size: "fill",
      border: "rounded",
      borderColor: theme.border,
      titleColor: theme.primary,
      footer: report
        ? `${report.metadata.cpu} · ${report.metadata.cores} cores · ${report.metadata.start_time}${report.metadata.quick ? " · quick run, not advertised" : ""}`
        : undefined,
      padding: [0, 1],
    },
    (panel) => {
      if (!report) {
        panel.label(state.benchError ?? "no benchmark yet", { fg: state.benchError ? theme.danger : theme.muted });
        panel.label("run `c0mpute bench` to measure this node and publish a score", { fg: theme.muted });
        panel.label(state.benchPath, { fg: theme.muted });
        return;
      }
      panel.row({ size: 1 }, (row) => {
        row.text(`score ${report.score.toLocaleString()}`, { fg: theme.accent, bold: true, size: 20 });
        row.text("1000 = one reference core on every workload", { fg: theme.muted, size: "fill" });
      });
      panel.spacer(1);
      const summaries = summarizeWorkloads(report);
      for (const s of summaries) {
        panel.row({ size: 1 }, (row) => {
          row.text(`${s.name} (${s.params})`, { fg: theme.foreground, bold: true, size: 20 });
          row.text(
            `${humanRate(s.name, s.best.result.throughput)} @ ${s.best.threads} thr`,
            { fg: theme.foreground, size: "fill" },
          );
        });
        panel.meter({
          value: Math.round(s.efficiency * 100),
          max: 100,
          label: "  scaling",
          text: `${Math.round(s.efficiency * 100)}% of ideal`,
          color: s.efficiency >= 0.7 ? theme.success : s.efficiency >= 0.4 ? theme.warning : theme.danger,
          labelWidth: 11,
          size: 1,
        });
        panel.sparkline({ values: s.speedups, color: theme.accent, label: "  speedup", size: 1 });
      }
    },
  );
}

function workersPanel(parent: Container, state: State, theme: Theme): void {
  parent.panel(
    { title: " WORKER ", size: 5, border: "rounded", borderColor: theme.border, titleColor: theme.primary, padding: [0, 1] },
    (panel) => {
      const w = state.worker;
      if (w.kind === "running") {
        panel.text(`running · pid ${w.pid}`, { fg: theme.success });
        panel.label("c0mpute worker status", { fg: theme.muted });
        panel.label("c0mpute worker stop", { fg: theme.muted });
      } else if (w.kind === "stale") {
        panel.text(`not running · stale pid ${w.pid}`, { fg: theme.warning });
        panel.label("c0mpute worker start -d", { fg: theme.muted });
      } else {
        panel.text("not running", { fg: theme.muted });
        panel.label("c0mpute worker register", { fg: theme.muted });
        panel.label("c0mpute worker start -d", { fg: theme.muted });
      }
    },
  );
}

function modulesPanel(parent: Container, state: State, theme: Theme): void {
  parent.panel(
    { title: " MODULES ", size: "fill", border: "rounded", borderColor: theme.border, titleColor: theme.primary, padding: [0, 1] },
    (panel) => {
      for (const m of state.modules) {
        panel.text(`▸ ${m}`, { fg: theme.foreground });
      }
      panel.spacer(1);
      panel.label("c0mpute plugin list", { fg: theme.muted });
    },
  );
}

export function renderDashboard(ui: Container, state: State, theme: Theme): void {
  ui.column({ size: "fill" }, (col) => {
    header(col, state, theme);
    col.row({ size: "fill", gap: 1 }, (row) => {
      row.column({ size: 34 }, (side) => {
        workersPanel(side, state, theme);
        modulesPanel(side, state, theme);
      });
      benchPanel(row, state, theme);
    });
    col.statusBar({
      items: [
        { key: "r", label: "reload" },
        { key: "q", label: "quit" },
      ],
      right: [{ key: "c0mpute bench", label: "measure this node" }],
      size: 1,
    });
  });
}
