/** Small drawing helpers shared by every tab. */
import { Attr, stringWidth, truncate, wrap, type Color, type Container, type Surface, type Theme } from "@profullstack/hqtui";

export interface Line {
  text: string;
  fg?: Color;
  bold?: boolean;
  dim?: boolean;
}

const SPIN = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";
export const spin = (now: number) => SPIN[Math.floor(now / 1000) % SPIN.length]!;

/** `████████░░░░ 62%` in `width` columns. */
export function bar(progress: number, width: number): string {
  const w = Math.max(4, width);
  const filled = Math.round(Math.max(0, Math.min(1, progress)) * w);
  return `${"█".repeat(filled)}${"░".repeat(w - filled)}`;
}

/** Wrap a block of text (keeping its own newlines) to `width`, indenting continuation lines. */
export function wrapBlock(text: string, width: number, indent = ""): string[] {
  const out: string[] = [];
  const w = Math.max(8, width - stringWidth(indent));
  for (const raw of text.split("\n")) {
    if (!raw.trim()) {
      out.push(indent);
      continue;
    }
    for (const l of wrap(raw.replace(/\t/g, "  "), w)) out.push(indent + l);
  }
  return out;
}

/** Keep the END of a path: `…/Downloads/c0mpute/a.png`. */
export function tail(text: string, max: number): string {
  if (stringWidth(text) <= max) return text;
  if (max <= 1) return "…";
  let out = "";
  for (const ch of Array.from(text).reverse()) {
    if (stringWidth(ch + out) > max - 1) break;
    out = ch + out;
  }
  return `…${out}`;
}

/** Draw `lines` bottom-anchored, scrolled up by `scroll`; returns the clamped scroll. */
export function drawLines(surface: Surface, lines: Line[], scroll: number, theme: Theme, fromBottom = true): number {
  const h = surface.height;
  const max = Math.max(0, lines.length - h);
  const s = Math.min(scroll, max);
  const start = fromBottom ? Math.max(0, lines.length - h - s) : Math.min(s, max);
  for (let y = 0; y < h; y++) {
    const l = lines[start + y];
    if (!l) break;
    surface.text(0, y, truncate(l.text, surface.width), {
      fg: l.fg ?? theme.foreground,
      attrs: (l.bold ? Attr.Bold : 0) | (l.dim ? Attr.Dim : 0),
    });
  }
  if (max > 0 && surface.width > 2) {
    // A one-column scroll hint on the right edge.
    const pos = fromBottom ? 1 - s / max : s / max;
    const y = Math.round(pos * (h - 1));
    surface.text(surface.width - 1, y, "▐", { fg: theme.muted });
  }
  return s;
}

/** A labelled panel with our border colours. */
export function panel(
  parent: Container,
  theme: Theme,
  title: string,
  opts: { size?: number | string; subtitle?: string; footer?: string; focused?: boolean },
  build: (p: Container) => void,
): void {
  parent.panel(
    {
      title: ` ${title} `,
      subtitle: opts.subtitle,
      footer: opts.footer,
      size: (opts.size ?? "fill") as never,
      border: "rounded",
      borderColor: opts.focused ? theme.borderFocused : theme.border,
      titleColor: theme.primary,
      subtitleColor: theme.muted,
      padding: [0, 1],
    },
    build,
  );
}

export function clock(d: Date): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

export function duration(seconds: number): string {
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m${String(s % 60).padStart(2, "0")}s`;
  if (s < 86400) return `${Math.floor(s / 3600)}h${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}m`;
  return `${Math.floor(s / 86400)}d${Math.floor((s % 86400) / 3600)}h`;
}
