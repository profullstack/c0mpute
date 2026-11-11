/**
 * Settings: the c0mpute.com sign-in, the default model, where results are
 * saved, the 18+ mode exactly as the web chat offers it, image previews and
 * the hosting node /push uses.
 */
import type { Container, Theme } from "@profullstack/hqtui";
import { expandHome } from "../config.ts";
import { usd } from "../network.ts";
import { adultActive, SETTINGS_ROWS, type AppState, type SettingsRow } from "../state.ts";
import { panel } from "./common.ts";

export interface SettingsHandlers {
  onSelect?: (index: number) => void;
}

export interface SettingsItem {
  row: SettingsRow;
  label: string;
  value: string;
  hint: string;
  warn?: boolean;
}

/** Every row's label, current value and what Enter does (pure; tested). */
export function settingsItems(state: AppState): SettingsItem[] {
  const me = state.me.data;
  const s = state.settings;
  const signedIn = !!state.creds.key;
  const verified = !!me?.adult_verified_at;
  return SETTINGS_ROWS.map((row): SettingsItem => {
    switch (row) {
      case "account":
        return signedIn
          ? {
              row,
              label: "account",
              value: me
                ? `${me.name ?? "(no name)"} · ${me.did.slice(0, 32)}${me.did.length > 32 ? "…" : ""} · ${me.balance ? `balance ${usd(me.balance.spendable_usd)}` : "free tier"}`
                : state.me.error
                  ? `key from ${state.creds.source === "env" ? "$C0MPUTE_API_KEY" : "~/.c0mpute/chat.json"}; ${state.me.error}`
                  : "checking…",
              hint: `${state.creds.url} · Enter re-checks · sign out: c0mpute chat logout`,
              warn: !me && !!state.me.error,
            }
          : { row, label: "account", value: "not signed in", hint: "L or Enter: sign in here (device code, approve with CoinPay) · or run: c0mpute chat login", warn: true };
      case "model":
        return { row, label: "default model", value: s.defaultModel ?? "any live model", hint: "Enter: pick (new chats use it; /model changes one chat)" };
      case "downloads":
        return { row, label: "downloads", value: expandHome(s.downloadsDir), hint: "Enter: edit · results are saved here" };
      case "adult":
        return {
          row,
          label: "18+ mode",
          value: verified ? `${s.adultMode ? "on" : "off"} for this terminal${adultActive(state) ? "" : ""}` : "off (account not 18+ verified)",
          hint: verified
            ? "Enter: toggle · X: remove 18+ verification · needs a paid balance; never pushed or shared"
            : "verify on c0mpute.com/chat (chat settings): an API key cannot attest for a person",
        };
      case "preview":
        return {
          row,
          label: "image preview",
          value: s.preview === "auto" ? (state.imagesInTerminal ? "auto: in the terminal" : "auto: open with the desktop viewer") : s.preview === "open" ? "open with the desktop viewer" : "off",
          hint: "Enter: cycle auto / open / off · in-terminal needs kitty graphics (HQTUI_IMAGES=1 under tmux/SSH)",
        };
      case "pushNode":
        return { row, label: "push node", value: s.pushNode ?? "most recently seen", hint: "/connect hosting <node> picks one · Enter: reset" };
    }
  });
}

export function renderSettings(ui: Container, state: AppState, theme: Theme, h: SettingsHandlers = {}): void {
  const items = settingsItems(state);
  const sel = items[state.settingsSel];
  ui.column({ size: "fill" }, (col) => {
    panel(col, theme, "Settings", { subtitle: "↑/↓ select · Enter change" }, (p) => {
      p.table({
        rows: items.map((i) => ({ label: i.label, value: i.value, warn: i.warn })),
        columns: [
          { key: "label", title: "", width: 15 },
          { key: "value", title: "", width: "1fr" },
        ],
        header: false,
        selected: state.settingsSel,
        rowColor: (r) => (r.warn ? theme.warning : undefined),
        onSelectRow: (i) => h.onSelect?.(i),
        size: items.length,
      });
      p.spacer(1);
      if (state.editing) {
        p.text("downloads dir (Enter saves, Esc cancels):", { fg: theme.primary, size: 1 });
        p.textInput({ value: state.editing.value, cursor: state.editing.cursor, focused: true, size: 1 });
      } else if (sel) {
        p.text(sel.hint, { fg: theme.muted, wrap: true, size: 2 });
      }
      if (state.login) {
        p.spacer(1);
        p.heading("Sign in to c0mpute.com", { size: 1 });
        if (state.login.link) {
          p.text(`Open: ${state.login.link}`, { fg: theme.accent, wrap: true, size: 2 });
          p.text(`and check it shows the code  ${state.login.code}`, { fg: theme.foreground, size: 1 });
        }
        p.text(state.login.status, { fg: state.login.done ? theme.success : theme.muted, wrap: true, size: 2 });
        p.label("CoinPay sign-in; your CoinPay account needs a DID. The key is saved where `c0mpute chat` reads it.", { wrap: true, size: 2 });
      }
    });
  });
}
