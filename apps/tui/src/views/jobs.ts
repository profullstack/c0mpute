/**
 * Jobs: every image, clip, song, album and promo started here, live while
 * the network works on it, then the saved files.
 */
import type { Container, Theme } from "@profullstack/hqtui";
import { KIND_LABEL, mmss, type Job } from "../jobs.ts";
import type { AppState } from "../state.ts";
import { jobLines } from "./chat.ts";
import { drawLines, duration, panel, wrapBlock } from "./common.ts";

export interface JobsHandlers {
  onSelect?: (index: number) => void;
  onScroll?: (delta: number) => void;
}

const STATUS_MARK: Record<Job["status"], string> = {
  starting: "…",
  pending: "~",
  saving: "↓",
  done: "✓",
  failed: "!",
  stopped: "-",
};

export function jobRow(job: Job, now: number) {
  const secs = Math.round(((job.finishedAt ?? now) - job.createdAt) / 1000);
  const live = job.status === "pending" || job.status === "starting" || job.status === "saving";
  return {
    st: STATUS_MARK[job.status],
    kind: KIND_LABEL[job.kind],
    what: job.title ?? job.prompt,
    progress: job.status === "done" ? "100%" : live && job.progress !== null ? `${Math.round(job.progress * 100)}%` : live ? "…" : job.status,
    eta: live && job.etaS !== null ? mmss(job.etaS) : "",
    age: duration(secs),
    stage: job.status === "failed" ? (job.error ?? "failed") : job.stage,
  };
}

export function renderJobs(ui: Container, state: AppState, theme: Theme, h: JobsHandlers = {}): void {
  const jobs = state.jobs;
  const sel = jobs[state.jobSel];
  const wide = ui.width >= 120;
  const list = (parent: Container) =>
    panel(parent, theme, "Jobs", { size: wide ? "55%" : "45%", subtitle: `${jobs.filter((j) => j.status === "pending").length} running · ${jobs.length} total` }, (p) => {
      if (!jobs.length) {
        p.label("Nothing yet. In Chat: /image, /video, /music, /album or /promo.", { fg: theme.muted });
        p.label("Jobs survive a restart: the TUI picks the polling back up.", { fg: theme.muted });
        return;
      }
      p.table({
        rows: jobs.map((j) => jobRow(j, state.now)),
        columns: [
          { key: "st", title: "", width: 1 },
          { key: "kind", title: "kind", width: 6 },
          { key: "what", title: "what", width: "1fr" },
          { key: "progress", title: "prog", width: 7, align: "right" },
          { key: "eta", title: "eta", width: 5, align: "right" },
          { key: "age", title: "time", width: 7, align: "right" },
        ],
        selected: state.jobSel,
        followSelection: true,
        scrollbar: true,
        rowColor: (row) => (row.st === "!" ? theme.danger : row.st === "✓" ? theme.success : row.st === "~" || row.st === "↓" ? theme.warning : undefined),
        onSelectRow: (i) => h.onSelect?.(i),
        onScroll: (d) => h.onScroll?.(d),
        size: "fill",
      });
    });
  const detail = (parent: Container) =>
    panel(parent, theme, sel ? `${KIND_LABEL[sel.kind]} ${sel.id.slice(0, 8)}` : "Detail", { size: "fill", subtitle: sel ? sel.status : undefined }, (p) => {
      if (!sel) {
        p.label("Select a job with ↑/↓.", { fg: theme.muted });
        return;
      }
      p.draw((surface) => {
        const lines = [
          { text: `asked: ${sel.prompt}`, fg: theme.muted },
          ...jobLines(sel, surface.width - 1, theme, state.now, "  "),
          { text: "" },
          ...wrapBlock(
            `r retry (new job) · ${sel.status === "pending" ? "x stop watching · " : ""}${sel.files.length ? "o open · " : ""}${sel.files.some((f) => /\.(png|jpe?g)$/i.test(f.path)) ? "p preview · " : ""}d remove from list`,
            surface.width - 1,
          ).map((text) => ({ text, fg: theme.muted })),
          ...(sel.status === "pending"
            ? wrapBlock("The API has no cancel: a stopped job may still finish (and be billed).", surface.width - 1).map((text) => ({ text, fg: theme.muted }))
            : []),
        ];
        drawLines(surface, lines, 0, theme, false);
      });
    });
  if (wide) {
    ui.row({ size: "fill", gap: 1 }, (row) => {
      list(row);
      detail(row);
    });
  } else {
    ui.column({ size: "fill" }, (col) => {
      list(col);
      detail(col);
    });
  }
}
