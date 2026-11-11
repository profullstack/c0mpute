// Values for MCP `${VAR}` placeholders that are not in the environment,
// encrypted with Electron safeStorage (the OS keychain / libsecret) into
// ~/.c0mpute/desktop-secrets.json. Values never go back to the renderer; it
// only learns which names are set.

import { safeStorage } from "electron";
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

const path = () => join(process.env.HOME || homedir(), ".c0mpute", "desktop-secrets.json");

type Stored = Record<string, { enc: string; plain?: false } | { value: string; plain: true }>;

function read(): Stored {
  if (!existsSync(path())) return {};
  try {
    return JSON.parse(readFileSync(path(), "utf8"));
  } catch {
    return {};
  }
}

function write(s: Stored) {
  mkdirSync(dirname(path()), { recursive: true });
  const tmp = `${path()}.tmp`;
  writeFileSync(tmp, JSON.stringify(s, null, 2), { mode: 0o600 });
  renameSync(tmp, path());
}

export function secretNames(): string[] {
  return Object.keys(read());
}

export function getSecret(name: string): string | undefined {
  const e = read()[name];
  if (!e) return undefined;
  if (e.plain) return e.value;
  try {
    return safeStorage.decryptString(Buffer.from(e.enc, "base64"));
  } catch {
    return undefined;
  }
}

/** Store (or with an empty value, remove) a secret. */
export function setSecret(name: string, value: string): { encrypted: boolean } {
  if (!/^[A-Za-z_][A-Za-z0-9_]{0,127}$/.test(name)) throw new Error("secret name: an environment variable name");
  const s = read();
  if (!value) {
    delete s[name];
    write(s);
    return { encrypted: true };
  }
  if (safeStorage.isEncryptionAvailable()) {
    s[name] = { enc: safeStorage.encryptString(value).toString("base64") };
    write(s);
    return { encrypted: true };
  }
  // No keyring (a headless box): the file is still 0600, and the UI says so.
  s[name] = { value, plain: true };
  write(s);
  return { encrypted: false };
}

/** Environment first, then the secret store. */
export function lookupVar(name: string): string | undefined {
  const v = process.env[name];
  return v !== undefined && v !== "" ? v : getSecret(name);
}
