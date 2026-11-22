/**
 * Fleet: every node linked to this account, what each last reported, and
 * actions on a selection. Every action is confirmed first (y/N, No by
 * default); node-side ones run on each node's next poll.
 */
import { truncate, type Container, type Theme } from "@profullstack/hqtui";
import { FLEET_ACTIONS, fleetRow, nodeDetail, type FleetOutcome } from "../fleet.ts";
import type { AppState } from "../state.ts";
import { clock, panel, wrapBlock } from "./common.ts";

export interface FleetHandlers {
  onCancel?: () => void;
  onSelect?: (index: number) => void;
  onScroll?: (delta: number) => void;
}

/** The same sign-in hint the Settings account row gives. */
export const FLEET_SIGN_IN = "Not signed in. L: sign in here (device code; approve with CoinPay, a passkey or a password) · or run: c0mpute login";

/** The result lines after an action (pure; tested). */
export function outcomeLines(o: FleetOutcome): string[] {
  const head = o.refused
    ? `${o.action}: every node refused`
    : `${o.action}: ${o.ok} ok${o.failed ? `, ${o.failed} failed` : ""}${o.nodeSide ? " · node-side actions run on each node's next poll (about a minute)" : ""}`;
  return [head, ...o.results.map((r) => `${r.ok ? "✓" : "✗"} ${r.node}${r.detail ? `  ${r.detail}` : ""}`)];
}

export function renderFleet(ui: Container, state: AppState, theme: Theme, h: FleetHandlers = {}): void {
  const f = state.fleet;
  ui.column({ size: "fill" }, (col) => {
    if (!state.creds.key) {
      panel(col, theme, "Fleet", {}, (p) => {
        p.text(FLEET_SIGN_IN, { fg: theme.warning, wrap: true, size: 3 });
      });
      return;
    }
    const nodes = f.list.data?.nodes ?? [];
    const online = nodes.filter((n) => n.online).length;
    const stamp = f.list.loading && !f.list.at ? "loading…" : f.list.error ? (f.list.at && f.list.data ? `stale since ${clock(new Date(f.list.at))}: ${f.list.error}` : f.list.error) : f.list.at ? `as of ${clock(new Date(f.list.at))}` : "";
    const subtitle = `${nodes.length} nodes · ${online} online${f.picked.length ? ` · ${f.picked.length} selected` : ""} · ${stamp}`;
    panel(col, theme, "Fleet", { size: "55%", subtitle }, (p) => {
      if (!nodes.length) {
        p.label(f.list.error ?? (f.list.at ? "No nodes linked to this account yet. On a machine: c0mpute node link" : "loading the fleet…"), { fg: f.list.error ? theme.warning : theme.muted, wrap: true });
        return;
      }
      p.table({
        rows: nodes.map((n) => fleetRow(n, f.picked.includes(n.node))),
        columns: [
          { key: "pick", title: "", width: 1 },
          { key: "dot", title: "", width: 1 },
          { key: "node", title: "node", width: 8 },
          { key: "label", title: "label", width: "1fr" },
          { key: "version", title: "ver", width: 8 },
          { key: "state", title: "state", width: 7 },
          { key: "tags", title: "tags", width: 14 },
          { key: "jobs", title: "jobs", width: 5, align: "right" },
          { key: "earned", title: "24h/30d", width: 13, align: "right" },
        ],
        selected: f.sel,
        followSelection: true,
        scrollbar: true,
        rowColor: (r) => (r.paused ? theme.warning : r.online ? undefined : theme.muted),
        onSelectRow: (i) => h.onSelect?.(i),
        onScroll: (d) => h.onScroll?.(d),
        size: "fill",
      });
    });
    const sel = nodes[f.sel];
    panel(col, theme, sel ? `${sel.node}${sel.label ? ` ${sel.label}` : ""}` : "Detail", { size: "fill", subtitle: f.sending ? `sending ${f.sending}…` : undefined }, (p) => {
      p.draw((surface) => {
        const w = surface.width - 1;
        const lines: { text: string; fg?: typeof theme.foreground }[] = [];
        if (f.outcome) {
          for (const [i, l] of outcomeLines(f.outcome).entries()) {
            for (const t of wrapBlock(l, w)) lines.push({ text: t, fg: i === 0 ? (f.outcome.refused || f.outcome.failed ? theme.danger : theme.success) : l.startsWith("✗") ? theme.danger : theme.foreground });
          }
          lines.push({ text: "" });
        }
        if (sel) for (const l of nodeDetail(sel)) for (const t of wrapBlock(l, w)) lines.push({ text: t, fg: l.startsWith("update to") ? theme.danger : theme.foreground });
        lines.push({ text: "" });
        for (const t of wrapBlock("Space select · a all · Enter actions · Esc clear · actions apply to the selection, else the highlighted node", w)) lines.push({ text: t, fg: theme.muted });
        lines.slice(0, surface.height).forEach((l, y) => surface.text(0, y, truncate(l.text, surface.width), { fg: l.fg ?? theme.foreground }));
      });
    });
  });
}

/** The action menu, the tags/models prompt and the y/N confirmation, as modals. */
export function fleetOverlay(ui: Container, state: AppState, theme: Theme, onDismiss: () => void): boolean {
  const f = state.fleet;
  const width = Math.min(ui.width - 4, 80);
  if (f.confirm) {
    const isToggle = "on" in f.confirm.req.args;
    const lines = wrapBlock(f.confirm.text, width - 4);
    ui.modal({ title: " confirm ", width, height: lines.length + 6, onDismiss }, (m) => {
      for (const l of lines) m.text(l, { fg: theme.warning, bold: true, size: 1 });
      m.spacer(1);
      m.label(`y: send · n, Enter or Esc: cancel${isToggle ? " · t: set the other value" : ""}`, { fg: theme.muted, size: 1 });
    });
    return true;
  }
  if (f.prompt) {
    const p = f.prompt;
    const what = p.action === "models" ? "models to serve, comma separated (empty = every model the node has)" : p.action === "tag" ? "tags to add, comma separated" : "tags to remove, comma separated";
    ui.modal({ title: ` ${p.action} · ${p.nodes.length} node${p.nodes.length === 1 ? "" : "s"} `, width, height: 7, onDismiss }, (m) => {
      m.text(what, { fg: theme.primary, size: 1 });
      m.textInput({ value: p.input.value, cursor: p.input.cursor, focused: true, size: 1 });
      m.label("Enter: next (you confirm before anything is sent) · Esc: cancel", { fg: theme.muted, size: 1 });
    });
    return true;
  }
  if (f.menu !== null) {
    ui.modal({ title: " fleet actions ", width: Math.min(ui.width - 4, 50), height: Math.min(ui.height - 2, FLEET_ACTIONS.length + 5), onDismiss }, (m) => {
      m.list({ items: FLEET_ACTIONS.map((a) => `${a.key}  ${a.label}`), selected: f.menu ?? 0, followSelection: true, size: "fill" });
      m.label("↑/↓ Enter or the letter · Esc closes", { fg: theme.muted, size: 1 });
    });
    return true;
  }
  return false;
}
