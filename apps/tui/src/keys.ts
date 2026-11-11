/**
 * Every key, in one place. Returns "quit" when the app should exit.
 *
 * In the chat input keys type; Esc leaves the input so single letters work
 * as shortcuts there too. Everywhere else single letters are shortcuts.
 */
import { editText, matchKey, type KeyEvent } from "@profullstack/hqtui";
import type { Controller } from "./controller.ts";
import { SETTINGS_ROWS, TABS } from "./state.ts";

const is = (e: KeyEvent, ...keys: string[]) => keys.some((k) => matchKey(e, k));

export function handleKey(ctl: Controller, e: KeyEvent): "quit" | void {
  const s = ctl.state;
  if (is(e, "ctrl+c")) return "quit";

  // Overlays own the keyboard while they are up.
  if (s.picker) {
    if (is(e, "up", "k")) ctl.movePicker(-1);
    else if (is(e, "down", "j")) ctl.movePicker(1);
    else if (is(e, "pageup")) ctl.movePicker(-10);
    else if (is(e, "pagedown")) ctl.movePicker(10);
    else if (is(e, "enter")) ctl.choosePicker();
    else if (is(e, "escape", "q")) ctl.closeOverlays();
    return;
  }
  if (s.preview) {
    if (is(e, "o")) ctl.openJob(s.jobs.find((j) => j.id === s.preview!.jobId));
    ctl.closeOverlays();
    return;
  }
  if (s.help) {
    if (is(e, "escape", "?", "q", "enter", "f1")) ctl.closeOverlays();
    return;
  }
  if (s.editing) {
    if (is(e, "enter")) ctl.commitEditing();
    else if (is(e, "escape")) ctl.setEditing(null);
    else {
      const next = editText(s.editing, e) ?? (e.char && !e.ctrl && !e.alt ? { value: s.editing.value.slice(0, s.editing.cursor) + e.char + s.editing.value.slice(s.editing.cursor), cursor: s.editing.cursor + e.char.length } : null);
      if (next) ctl.setEditing(next);
    }
    return;
  }

  // Global, even while typing.
  if (is(e, "tab")) return ctl.cycleTab(1);
  if (is(e, "shift+tab")) return ctl.cycleTab(-1);
  if (is(e, "f1")) return ctl.toggleHelp();
  for (let i = 0; i < TABS.length; i++) if (is(e, `alt+${i + 1}`, `f${i + 2}`)) return ctl.setTab(TABS[i]!);

  if (s.tab === "chat" && s.chat.typing) return chatTyping(ctl, e);

  // Shortcuts.
  if (s.login && is(e, "escape")) return ctl.cancelLogin();
  if (is(e, "q")) return "quit";
  if (is(e, "?")) return ctl.toggleHelp();
  if (e.char && /^[1-5]$/.test(e.char) && !e.ctrl && !e.alt) return ctl.setTab(TABS[Number(e.char) - 1]!);

  switch (s.tab) {
    case "chat":
      return chatKeys(ctl, e);
    case "jobs":
      return jobsKeys(ctl, e);
    case "node":
      if (is(e, "r")) {
        ctl.refreshWorker();
        void ctl.refreshStatus();
        void ctl.refreshLogs();
        void ctl.refreshVersion();
        ctl.toast("re-checking the node (c0mpute status --json)…");
      } else if (is(e, "b")) ctl.toggleBench();
      else if (is(e, "up", "k")) ctl.scrollNode(-1);
      else if (is(e, "down", "j")) ctl.scrollNode(1);
      else if (is(e, "pageup")) ctl.scrollNode(-10);
      else if (is(e, "pagedown")) ctl.scrollNode(10);
      else if (is(e, "home")) ctl.scrollNode(-1e6);
      return;
    case "network":
      if (is(e, "r")) {
        void ctl.refreshNetwork();
        void ctl.refreshAccount();
        ctl.toast("refreshing the network…");
      } else if (is(e, "v", "p")) ctl.toggleNetPanel();
      else if (is(e, "up", "k")) ctl.scrollNet(-1);
      else if (is(e, "down", "j")) ctl.scrollNet(1);
      else if (is(e, "pageup")) ctl.scrollNet(-10);
      else if (is(e, "pagedown")) ctl.scrollNet(10);
      else if (is(e, "home")) ctl.scrollNet(-1e6);
      return;
    case "settings":
      if (is(e, "up", "k")) ctl.moveSetting(-1);
      else if (is(e, "down", "j")) ctl.moveSetting(1);
      else if (is(e, "enter", "space")) ctl.activateSetting();
      else if (e.char === "L" || e.char === "l") void ctl.login();
      else if ((e.char === "X" || e.char === "x") && SETTINGS_ROWS[s.settingsSel] === "adult") void ctl.revokeAdult();
      else if (is(e, "r")) void ctl.refreshAccount();
      return;
  }
}

function chatTyping(ctl: Controller, e: KeyEvent): void {
  const s = ctl.state;
  if (is(e, "enter")) return void ctl.submit();
  if (is(e, "escape")) {
    if (s.chat.streaming) return ctl.stopStreaming();
    return ctl.setTyping(false);
  }
  if (is(e, "pageup")) return ctl.scrollChat(Math.max(3, s.rows - 8));
  if (is(e, "pagedown")) return ctl.scrollChat(-Math.max(3, s.rows - 8));
  if (is(e, "up")) {
    if (!s.chat.input.value && ctl.unqueueLast()) return;
    return ctl.recall(-1);
  }
  if (is(e, "down")) return ctl.recall(1);
  if (is(e, "ctrl+n")) return ctl.newChat();
  const next = editText(s.chat.input, e);
  if (next) return ctl.setInput(next);
  if (e.char && !e.ctrl && !e.alt) {
    const { value, cursor } = s.chat.input;
    ctl.setInput({ value: value.slice(0, cursor) + e.char + value.slice(cursor), cursor: cursor + e.char.length });
  }
}

function lastJobWithFiles(ctl: Controller) {
  const ids = ctl.state.chat.convo.messages.map((m) => m.jobId).filter(Boolean).reverse();
  for (const id of ids) {
    const j = ctl.state.jobs.find((x) => x.id === id);
    if (j?.files.length) return j;
  }
  return undefined;
}

function chatKeys(ctl: Controller, e: KeyEvent): void {
  const s = ctl.state;
  if (is(e, "i", "enter", "a")) return ctl.setTyping(true);
  if (e.char === "/") {
    ctl.setInput({ value: "/", cursor: 1 });
    return ctl.setTyping(true);
  }
  if (is(e, "escape") && s.chat.streaming) return ctl.stopStreaming();
  if (is(e, "m")) return ctl.openPicker();
  if (is(e, "n")) return ctl.newChat();
  if (is(e, "p")) return ctl.previewJob(lastJobWithFiles(ctl), previewSize(s.rows));
  if (is(e, "o")) return ctl.openJob(lastJobWithFiles(ctl));
  if (is(e, "r")) return ctl.refreshAll();
  if (is(e, "up", "k")) return ctl.scrollChat(1);
  if (is(e, "down", "j")) return ctl.scrollChat(-1);
  if (is(e, "pageup")) return ctl.scrollChat(Math.max(3, s.rows - 8));
  if (is(e, "pagedown")) return ctl.scrollChat(-Math.max(3, s.rows - 8));
  if (is(e, "end", "G")) return ctl.scrollChat(-1e6);
}

export function previewSize(rows: number): { cols: number; rows: number } {
  const r = Math.max(6, Math.min(18, rows - 6));
  return { cols: Math.min(60, r * 2 + 20), rows: r };
}

function jobsKeys(ctl: Controller, e: KeyEvent): void {
  const s = ctl.state;
  if (is(e, "up", "k")) ctl.moveJob(-1);
  else if (is(e, "down", "j")) ctl.moveJob(1);
  else if (is(e, "pageup")) ctl.moveJob(-10);
  else if (is(e, "pagedown")) ctl.moveJob(10);
  else if (is(e, "home", "g")) ctl.moveJob(-1e6);
  else if (is(e, "end", "G")) ctl.moveJob(1e6);
  else if (is(e, "o", "enter")) ctl.openJob();
  else if (is(e, "p")) ctl.previewJob(undefined, previewSize(s.rows));
  else if (is(e, "r")) void ctl.retryJob();
  else if (is(e, "x")) ctl.stopJob();
  else if (is(e, "d", "delete")) ctl.removeJob();
}
