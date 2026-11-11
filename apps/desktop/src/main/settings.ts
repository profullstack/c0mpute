// The app's own settings, ~/.c0mpute/desktop.json. Nothing secret lives here:
// MCP `${VAR}` values go in the safeStorage-encrypted secrets file.

import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

export interface Settings {
  /** MCP servers whose tools run without asking each time. */
  mcpAllowed: string[];
  /** Skill folder names the agent does not see. */
  skillsDisabled: string[];
  /** Skill scripts run without asking (off by default). */
  skillScriptsAllowed: boolean;
  /** The built-in shell tool (off by default; every command is confirmed). */
  shellEnabled: boolean;
  agentModel: string;
  /** `c0mpute chat connect hosting --name <node>` kept running. */
  connectHosting: { enabled: boolean; name: string };
  /** Worker start flags the Node tab uses. */
  worker: { noStorage: boolean; noHosting: boolean; private: boolean; roles: string };
  /** Media job ids seen in the chat, to keep polling across restarts. */
  mediaJobs: { kind: string; id: string; at: number }[];
}

export const DEFAULT_SETTINGS: Settings = {
  mcpAllowed: [],
  skillsDisabled: [],
  skillScriptsAllowed: false,
  shellEnabled: false,
  agentModel: "",
  connectHosting: { enabled: false, name: "" },
  worker: { noStorage: false, noHosting: false, private: false, roles: "" },
  mediaJobs: [],
};

export function settingsPath(): string {
  return join(process.env.HOME || homedir(), ".c0mpute", "desktop.json");
}

export function loadSettings(path = settingsPath()): Settings {
  if (!existsSync(path)) return structuredClone(DEFAULT_SETTINGS);
  try {
    const raw = JSON.parse(readFileSync(path, "utf8")) as Partial<Settings>;
    return {
      ...structuredClone(DEFAULT_SETTINGS),
      ...raw,
      connectHosting: { ...DEFAULT_SETTINGS.connectHosting, ...(raw.connectHosting ?? {}) },
      worker: { ...DEFAULT_SETTINGS.worker, ...(raw.worker ?? {}) },
    };
  } catch {
    return structuredClone(DEFAULT_SETTINGS);
  }
}

export function saveSettings(s: Settings, path = settingsPath()): void {
  mkdirSync(dirname(path), { recursive: true });
  const tmp = `${path}.tmp-${process.pid}`;
  writeFileSync(tmp, `${JSON.stringify(s, null, 2)}\n`, { mode: 0o600 });
  renameSync(tmp, path);
}
