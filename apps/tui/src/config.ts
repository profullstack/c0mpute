/**
 * Where the TUI keeps what it remembers, and the c0mpute.com sign-in it shares
 * with `c0mpute chat`.
 *
 *   ~/.c0mpute/chat.json      the `c0mpute chat login` key (shared with the CLI)
 *   ~/.c0mpute/tui.json       settings (default model, downloads dir, 18+ mode)
 *   ~/.c0mpute/tui-jobs.json  generation jobs, so a restart resumes polling
 *   ~/.c0mpute/tui-chat.json  the open conversation
 *
 * Not under ~/.c0mpute/tui/: the installer replaces that directory on update.
 */
import { chmodSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

export const DEFAULT_URL = "https://c0mpute.com";
export const INFERNET_URL = "https://infernetprotocol.com";

export function c0mputeHome(home = homedir()): string {
  return join(home, ".c0mpute");
}

export const paths = {
  creds: (home?: string) => join(c0mputeHome(home), "chat.json"),
  settings: (home?: string) => join(c0mputeHome(home), "tui.json"),
  jobs: (home?: string) => join(c0mputeHome(home), "tui-jobs.json"),
  chat: (home?: string) => join(c0mputeHome(home), "tui-chat.json"),
};

export function readJson<T>(path: string): T | null {
  try {
    return JSON.parse(readFileSync(path, "utf8")) as T;
  } catch {
    return null;
  }
}

/** Write atomically (tmp + rename), owner-only when `secret`. */
export function writeJson(path: string, value: unknown, secret = false): void {
  mkdirSync(dirname(path), { recursive: true });
  const tmp = `${path}.${process.pid}.tmp`;
  writeFileSync(tmp, JSON.stringify(value, null, 2), { mode: secret ? 0o600 : 0o644 });
  if (secret) chmodSync(tmp, 0o600);
  renameSync(tmp, path);
}

// ─── sign-in ────────────────────────────────────────────────────────────

export interface Creds {
  url: string;
  key: string | null;
  /** Where the key came from, for the Settings tab. */
  source: "env" | "file" | null;
}

/** Same precedence as the Rust CLI (ai_chat.rs): env, then chat.json, then the default. */
export function loadCreds(env: NodeJS.ProcessEnv = process.env, home?: string): Creds {
  const saved = readJson<{ url?: string | null; key?: string | null }>(paths.creds(home)) ?? {};
  const envKey = env.C0MPUTE_API_KEY?.trim();
  const key = envKey ? envKey : saved.key?.trim() || null;
  const url = (env.C0MPUTE_URL?.trim() || saved.url?.trim() || DEFAULT_URL).replace(/\/+$/, "");
  return { url, key, source: envKey ? "env" : key ? "file" : null };
}

/** Save a key the way `c0mpute chat login` does: `{url?, key}`, mode 0600. */
export function saveCreds(url: string, key: string, home?: string): string {
  const path = paths.creds(home);
  writeJson(path, { url: url === DEFAULT_URL ? null : url, key }, true);
  return path;
}

// ─── settings ───────────────────────────────────────────────────────────

export type PreviewMode = "auto" | "open" | "off";

export interface Settings {
  /** Model for new chats; null = any live model. */
  defaultModel: string | null;
  downloadsDir: string;
  /**
   * 18+ mode for this terminal, the TUI's twin of the web chat's per-browser
   * toggle. Only takes effect when the account itself is 18+ verified, which
   * is done in the c0mpute.com chat settings (an API key cannot attest).
   */
  adultMode: boolean;
  /** Images: draw in the terminal (kitty graphics) when it can, open with the desktop viewer, or neither. */
  preview: PreviewMode;
  /** The hosting node `/push hosting` publishes through (`/connect hosting <node>`); null = most recently seen. */
  pushNode: string | null;
}

export function defaultDownloadsDir(env: NodeJS.ProcessEnv = process.env, home = homedir()): string {
  const xdg = env.XDG_DOWNLOAD_DIR?.trim();
  return join(xdg || join(home, "Downloads"), "c0mpute");
}

export function loadSettings(env: NodeJS.ProcessEnv = process.env, home?: string): Settings {
  const s = readJson<Partial<Settings>>(paths.settings(home)) ?? {};
  return {
    defaultModel: typeof s.defaultModel === "string" && s.defaultModel ? s.defaultModel : null,
    downloadsDir: typeof s.downloadsDir === "string" && s.downloadsDir ? s.downloadsDir : defaultDownloadsDir(env, home),
    adultMode: s.adultMode === true,
    preview: s.preview === "open" || s.preview === "off" ? s.preview : "auto",
    pushNode: typeof s.pushNode === "string" && s.pushNode ? s.pushNode : null,
  };
}

export function saveSettings(settings: Settings, home?: string): void {
  writeJson(paths.settings(home), settings);
}

/** `~/x` → absolute. */
export function expandHome(p: string, home = homedir()): string {
  return p === "~" ? home : p.startsWith("~/") ? join(home, p.slice(2)) : p;
}
