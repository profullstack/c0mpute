/**
 * The whole screen: tab bar, the active tab, the key bar, and overlays (help,
 * model picker, image preview). A pure function of AppState; the handlers
 * are optional so tests render it without a controller.
 */
import { ansi256, placeholderCell, truncate, type Container, type Theme } from "@profullstack/hqtui";
import { HELP_COMMANDS } from "../commands.ts";
import { TAB_LABELS, TABS, type AppState, type Tab } from "../state.ts";
import { renderChat, type ChatHandlers } from "./chat.ts";
import { renderJobs, type JobsHandlers } from "./jobs.ts";
import { renderNetwork, type NetworkHandlers } from "./network.ts";
import { renderNode, type NodeHandlers } from "./node.ts";
import { renderSettings, type SettingsHandlers } from "./settings.ts";

export interface Handlers {
  onTab?: (tab: Tab) => void;
  onKey?: (key: string) => void;
  onPick?: (index: number) => void;
  onCloseOverlay?: () => void;
  chat?: ChatHandlers;
  jobs?: JobsHandlers;
  node?: NodeHandlers;
  network?: NetworkHandlers;
  settings?: SettingsHandlers;
}

/** The keys each tab answers to, for the bottom bar (and the help). */
export function tabKeys(state: AppState): { key: string; label: string }[] {
  switch (state.tab) {
    case "chat":
      return state.chat.typing
        ? [
            { key: "Enter", label: "send" },
            { key: "Esc", label: state.chat.streaming ? "stop" : "keys" },
            { key: "PgUp", label: "scroll" },
            { key: "Tab", label: "next tab" },
          ]
        : [
            { key: "i", label: "type" },
            { key: "m", label: "model" },
            { key: "n", label: "new chat" },
            { key: "p", label: "preview" },
            { key: "o", label: "open" },
            { key: "?", label: "help" },
          ];
    case "jobs":
      return [
        { key: "↑↓", label: "select" },
        { key: "o", label: "open" },
        { key: "p", label: "preview" },
        { key: "r", label: "retry" },
        { key: "x", label: "stop" },
        { key: "d", label: "remove" },
      ];
    case "node":
      return [
        { key: "r", label: "re-check" },
        { key: "b", label: state.nodePanel === "bench" ? "roles" : "bench" },
        { key: "↑↓", label: "scroll" },
      ];
    case "network":
      return [
        { key: "r", label: "refresh" },
        { key: "v", label: state.netPanel === "nodes" ? "prices" : "nodes" },
        { key: "↑↓", label: "scroll" },
      ];
    case "settings":
      return [
        { key: "↑↓", label: "select" },
        { key: "Enter", label: "change" },
        { key: "L", label: "sign in" },
      ];
  }
}

export const GLOBAL_KEYS: [string, string][] = [
  ["Tab / Shift+Tab", "next / previous tab"],
  ["1-5 or Alt+1-5", "go to a tab (Alt in the chat input)"],
  ["?  or  F1", "this help"],
  ["r", "refresh what this tab shows"],
  ["q / Ctrl+C", "quit (q when not typing)"],
  ["Esc", "close an overlay · leave the chat input · stop an answer"],
];

function header(ui: Container, state: AppState, theme: Theme, h: Handlers): void {
  ui.row({ size: 1 }, (row) => {
    row.text(" c0mpute ", { fg: theme.background, background: theme.primary, bold: true, size: 9 });
    row.tabs({
      tabs: TABS.map((t, i) => `${i + 1} ${TAB_LABELS[t]}${t === "jobs" ? jobsBadge(state) : ""}`),
      active: TABS.indexOf(state.tab),
      variant: "underline",
      color: theme.primary,
      onSelect: (i) => h.onTab?.(TABS[i]!),
      size: "fill",
    });
    const w = state.node.worker;
    const dot = w.state === "running" ? "● worker" : "○ worker";
    row.text(dot, { fg: w.state === "running" ? theme.success : theme.muted, size: 9, align: "right" });
  });
}

function jobsBadge(state: AppState): string {
  const n = state.jobs.filter((j) => j.status === "pending" || j.status === "starting" || j.status === "saving").length;
  return n ? ` (${n})` : "";
}

function footer(ui: Container, state: AppState, theme: Theme, h: Handlers): void {
  const t = state.toast;
  if (t) {
    ui.text(` ${t.text}`, { fg: t.kind === "error" ? theme.danger : t.kind === "ok" ? theme.success : theme.info, size: 1 });
    return;
  }
  ui.statusBar({
    items: tabKeys(state).map((k) => ({ key: k.key, label: k.label, onPress: () => h.onKey?.(k.key) })),
    right: [
      { key: "?", label: "help", onPress: () => h.onKey?.("?") },
      { key: "q", label: "quit", onPress: () => h.onKey?.("q") },
    ],
    size: 1,
  });
}

function helpOverlay(ui: Container, state: AppState, theme: Theme, h: Handlers): void {
  const width = Math.min(ui.width - 4, 96);
  const keys = tabKeys(state);
  const lines = [
    ...GLOBAL_KEYS.map(([k, d]) => `${k.padEnd(18)} ${d}`),
    "",
    `${TAB_LABELS[state.tab]}: ${keys.map((k) => `${k.key} ${k.label}`).join(" · ")}`,
    "",
    ...HELP_COMMANDS.map(([c, d]) => `${truncate(c, 30).padEnd(31)}${d}`),
  ];
  ui.modal(
    {
      title: " help ",
      width,
      height: Math.min(ui.height - 2, lines.length + 6),
      onDismiss: () => h.onCloseOverlay?.(),
      buttons: [{ label: "close", onPress: () => h.onCloseOverlay?.() }],
    },
    (m) => {
      m.draw((surface) => {
        lines.slice(0, surface.height).forEach((l, i) => surface.text(0, i, truncate(l, surface.width), { fg: l.startsWith("/") ? theme.accent : theme.foreground }));
      });
    },
  );
}

function pickerOverlay(ui: Container, state: AppState, theme: Theme, h: Handlers): void {
  const models = state.models.data ?? [];
  const items = ["any live model", ...models];
  ui.modal(
    {
      title: state.picker?.target === "default" ? " default model " : " model ",
      width: Math.min(ui.width - 4, 70),
      height: Math.min(ui.height - 2, items.length + 5),
      onDismiss: () => h.onCloseOverlay?.(),
    },
    (m) => {
      if (!models.length) m.label(state.models.error ?? "loading the live models…", { fg: theme.muted, size: 1 });
      m.list({
        items,
        selected: state.picker?.selected ?? 0,
        followSelection: true,
        scrollbar: true,
        onSelectRow: (i) => h.onPick?.(i),
        size: "fill",
      });
      m.label("↑/↓ Enter · Esc closes", { fg: theme.muted, size: 1 });
    },
  );
}

function previewOverlay(ui: Container, state: AppState, theme: Theme, h: Handlers): void {
  const p = state.preview!;
  ui.modal(
    {
      title: ` ${truncate(p.path.split("/").pop() ?? "image", 40)} `,
      width: Math.min(ui.width - 2, Math.max(40, p.cols + 4)),
      height: Math.min(ui.height - 1, p.rows + 5),
      onDismiss: () => h.onCloseOverlay?.(),
    },
    (m) => {
      m.draw((surface) => {
        if (p.imageId === null) {
          surface.text(0, 0, truncate(p.note ?? "no preview", surface.width), { fg: theme.muted });
          surface.text(0, 2, truncate(p.path, surface.width), { fg: theme.foreground });
          return;
        }
        // Kitty Unicode placeholders: the uploaded image shows in exactly
        // the cols x rows grid it was placed with.
        const fg = ansi256(p.imageId);
        for (let r = 0; r < Math.min(p.rows, surface.height - 1); r++) {
          for (let c = 0; c < Math.min(p.cols, surface.width); c++) surface.text(c, r, placeholderCell(r, c), { fg });
        }
        surface.text(0, surface.height - 1, truncate(`${p.path} · Esc closes · o opens`, surface.width), { fg: theme.muted });
      });
    },
  );
}

export function renderApp(ui: Container, state: AppState, theme: Theme, h: Handlers = {}): void {
  ui.column({ size: "fill" }, (col) => {
    header(col, state, theme, h);
    col.column({ size: "fill" }, (body) => {
      switch (state.tab) {
        case "chat":
          return renderChat(body, state, theme, h.chat);
        case "jobs":
          return renderJobs(body, state, theme, h.jobs);
        case "node":
          return renderNode(body, state, theme, h.node);
        case "network":
          return renderNetwork(body, state, theme, h.network);
        case "settings":
          return renderSettings(body, state, theme, h.settings);
      }
    });
    footer(col, state, theme, h);
  });
  if (state.picker) pickerOverlay(ui, state, theme, h);
  else if (state.preview) previewOverlay(ui, state, theme, h);
  else if (state.help) helpOverlay(ui, state, theme, h);
}
