/**
 * Chat: the c0mpute.com/chat conversation in a terminal. Answers stream in;
 * generation commands become turns whose live state is their job's.
 */
import type { Container, Theme } from "@profullstack/hqtui";
import { KIND_LABEL, mmss, type Job } from "../jobs.ts";
import { usd } from "../network.ts";
import { activeModel, adultActive, jobById, type AppState, type ChatMsg } from "../state.ts";
import { bar, drawLines, spin, tail, wrapBlock, type Line } from "./common.ts";

export interface ChatHandlers {
  onScroll?: (delta: number) => void;
  onFocusInput?: () => void;
}

/** One job, as lines under its chat turn (also used by the Jobs detail). */
export function jobLines(job: Job, width: number, theme: Theme, now: number, indent = "  "): Line[] {
  const out: Line[] = [];
  const secs = Math.round(((job.finishedAt ?? now) - job.createdAt) / 1000);
  const head = `${KIND_LABEL[job.kind]}${job.adult ? " (18+)" : ""}${job.title ? `: ${job.title}` : `: ${job.prompt}`}`;
  for (const l of wrapBlock(head, width, "")) out.push({ text: l, fg: theme.accent, bold: true });
  const live = job.status === "pending" || job.status === "starting" || job.status === "saving";
  const status = live ? `${spin(now)} ${job.stage} · ${mmss(secs)}` : job.status === "failed" ? `! ${job.error ?? "failed"}` : job.stage;
  const color = job.status === "failed" ? theme.danger : job.status === "done" ? theme.success : job.status === "stopped" ? theme.muted : theme.warning;
  for (const l of wrapBlock(status, width, indent)) out.push({ text: l, fg: color });
  if (live && job.progress !== null) {
    const pct = `${Math.round(job.progress * 100)}%`;
    const eta = job.etaS !== null && !/ left/.test(job.stage) ? ` · ~${mmss(job.etaS)} left` : "";
    const w = Math.max(8, Math.min(40, width - indent.length - pct.length - eta.length - 2));
    out.push({ text: `${indent}${bar(job.progress, w)} ${pct}${eta}`, fg: theme.primary });
  }
  if (job.worker && live) out.push({ text: `${indent}worker …${job.worker.slice(-8)}`, fg: theme.muted });
  for (const l of job.lines) for (const w of wrapBlock(l, width, indent)) out.push({ text: w, fg: theme.muted });
  for (const f of job.files) {
    out.push({ text: `${indent}saved ${f.label}: ${tail(f.path, Math.max(10, width - indent.length - f.label.length - 8))}`, fg: theme.foreground });
  }
  if (job.status === "done" && job.files.length) {
    const img = job.files.some((f) => /\.(png|jpe?g)$/i.test(f.path));
    out.push({ text: `${indent}${img ? "p preview · " : ""}o open · Jobs tab (Tab) for the rest`, fg: theme.muted, dim: true });
  }
  if (job.status === "done" && job.kind === "music" && job.lyrics) {
    for (const l of wrapBlock(job.lyrics.split("\n").slice(0, 12).join("\n"), width, indent)) out.push({ text: l, fg: theme.muted });
  }
  return out;
}

/** The transcript as wrapped, coloured lines (pure; tested). */
export function transcriptLines(state: AppState, width: number, theme: Theme): Line[] {
  const out: Line[] = [];
  const msgs = state.chat.convo.messages;
  if (!msgs.length) {
    out.push({ text: "Ask anything. Answers stream from a provider node on the c0mpute network.", fg: theme.muted });
    out.push({ text: "", fg: theme.muted });
    out.push({ text: "/image <prompt>   /video <prompt>   /music <style> [lyrics: …]", fg: theme.muted });
    out.push({ text: "/album <idea>     /promo <url> [style]   /push hosting <name>", fg: theme.muted });
    out.push({ text: "/model to pick a model · /help for everything · Esc then ? for keys", fg: theme.muted });
    if (!state.creds.key) {
      out.push({ text: "", fg: theme.muted });
      out.push({ text: "Not signed in: run `c0mpute chat login`, or press L in Settings (5).", fg: theme.warning });
    }
    return out;
  }
  msgs.forEach((m: ChatMsg, i) => {
    if (i > 0) out.push({ text: "" });
    if (m.role === "user") {
      const lines = wrapBlock(m.content, width - 6, "");
      lines.forEach((l, k) => out.push({ text: `${k === 0 ? "you › " : "      "}${l}`, fg: theme.accent }));
      return;
    }
    if (m.role === "note") {
      for (const l of wrapBlock(m.content, width, "// ")) out.push({ text: l, fg: m.error ? theme.danger : theme.muted });
      return;
    }
    const job = jobById(state, m.jobId);
    if (job) {
      const lines = jobLines(job, width - 4, theme, state.now);
      lines.forEach((l, k) => out.push({ ...l, text: `${k === 0 ? "c0 › " : "     "}${l.text}` }));
      return;
    }
    if (m.jobId) {
      out.push({ text: "c0 › (job no longer in the Jobs list)", fg: theme.muted });
      return;
    }
    const streaming = state.chat.streaming && i === msgs.length - 1;
    const body = m.content || (streaming ? `${spin(state.now)} waiting for a provider node…` : "");
    const lines = wrapBlock(body, width - 6, "");
    lines.forEach((l, k) => out.push({ text: `${k === 0 ? "c0 › " : "     "}${l}`, fg: m.error ? theme.danger : theme.foreground }));
    if (streaming && m.content) out.push({ text: `     ${spin(state.now)}`, fg: theme.muted });
  });
  return out;
}

export function renderChat(ui: Container, state: AppState, theme: Theme, h: ChatHandlers = {}): void {
  const me = state.me.data;
  const model = activeModel(state) ?? "any live model";
  ui.column({ size: "fill" }, (col) => {
    col.row({ size: 1, gap: 1 }, (row) => {
      row.text(`model ${model}`, { fg: theme.primary, size: "fill" });
      if (adultActive(state)) row.text("18+", { fg: theme.warning, size: 4 });
      const who = me ? `${me.name ?? me.did.slice(0, 24)} · ${me.balance ? `balance ${usd(me.balance.spendable_usd)}` : "free tier"}` : state.creds.key ? (state.me.error ? `offline? ${state.me.error}` : "checking sign-in…") : "not signed in · c0mpute chat login";
      row.text(who, { fg: me ? theme.muted : theme.warning, size: Math.min(48, Math.max(20, who.length)), align: "right" });
    });
    col.panel(
      {
        title: ` ${state.chat.convo.title} `,
        size: "fill",
        border: "rounded",
        borderColor: theme.border,
        titleColor: theme.primary,
        padding: [0, 1],
        onClick: () => h.onFocusInput?.(),
      },
      (p) => {
        p.draw((surface) => {
          const lines = transcriptLines(state, surface.width - 1, theme);
          drawLines(surface, lines, state.chat.scroll, theme, true);
          // Wheel up reads back through the conversation.
          p.ctx.hit({ rect: surface.hitRect(), onScroll: (d) => h.onScroll?.(-d * 3), onClick: () => h.onFocusInput?.() });
        });
      },
    );
    col.panel(
      {
        size: 3,
        border: "rounded",
        borderColor: state.chat.typing ? theme.borderFocused : theme.border,
        padding: [0, 1],
        title: state.chat.streaming ? " answering… Esc stops " : state.chat.typing ? undefined : " Esc'd: i or Enter to type ",
        titleColor: theme.muted,
        onClick: () => h.onFocusInput?.(),
      },
      (p) => {
        p.textInput({
          value: state.chat.input.value,
          cursor: state.chat.input.cursor,
          focused: state.chat.typing,
          placeholder: "message, /image, /video, /music, /album, /promo, /push hosting, /model, /help",
          size: 1,
        });
      },
    );
  });
}
