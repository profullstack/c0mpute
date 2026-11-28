// The c0mpute.com account this machine is signed in to. `c0mpute chat login`
// (= `c0mpute login`: device flow, approved on c0mpute.com with CoinPay, a
// passkey or a password) writes the key to ~/.c0mpute/chat.json (0600). The
// CLI and TUI keep reading that file. The desktop imports the key from it
// into its own store, ~/.c0mpute/desktop-account.json, encrypted with Electron
// safeStorage (the OS keychain: macOS Keychain, Windows DPAPI, libsecret or
// kwallet on Linux), and reads it from there afterwards. Without a keyring (a
// headless box) it keeps reading chat.json and says so.
// C0MPUTE_API_KEY / C0MPUTE_URL override both, as they do for the CLI
// (node/crates/c0mpute-cli/src/ai_chat.rs).
//
// This module never imports electron: index.ts hands it safeStorage through
// setKeychainBackend(), and tests pass a fake.

import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

export const DEFAULT_URL = "https://c0mpute.com";

export function chatJsonPath(home = process.env.HOME || homedir()): string {
  return join(home, ".c0mpute", "chat.json");
}

/** The desktop's keychain-encrypted store, next to chat.json. */
export function accountStorePath(chatJson = chatJsonPath()): string {
  return join(dirname(chatJson), "desktop-account.json");
}

/** The slice of Electron's safeStorage this module uses. */
export interface KeychainBackend {
  isEncryptionAvailable(): boolean;
  encryptString(s: string): Buffer;
  decryptString(b: Buffer): string;
}

let defaultBackend: KeychainBackend | null = null;
/** index.ts sets Electron's safeStorage here once the app is ready. */
export function setKeychainBackend(b: KeychainBackend | null) {
  defaultBackend = b;
}

export interface Creds {
  url: string;
  key?: string;
  source: "env" | "keychain" | "chat.json" | "none";
  /** True when the OS keychain is usable for the account key. */
  keychain: boolean;
}

export interface CredsOptions {
  /** Defaults to accountStorePath(chat.json path). */
  store?: string;
  /** Defaults to the backend set with setKeychainBackend (none under bun). */
  backend?: KeychainBackend | null;
}

interface StoreFile {
  url: string;
  enc: string;
  imported_from_mtime_ms: number;
  fingerprint: string;
}

/** A short sha256 prefix that identifies a key without revealing it. */
export function keyFingerprint(key: string): string {
  return createHash("sha256").update(key).digest("hex").slice(0, 16);
}

function available(b: KeychainBackend | null | undefined): b is KeychainBackend {
  if (!b) return false;
  try {
    return b.isEncryptionAvailable();
  } catch {
    return false;
  }
}

function readChatJson(path: string): { url?: string; key?: string; mtimeMs: number } {
  if (!existsSync(path)) return { mtimeMs: 0 };
  try {
    const mtimeMs = statSync(path).mtimeMs;
    const j = JSON.parse(readFileSync(path, "utf8"));
    return {
      url: typeof j?.url === "string" ? j.url : undefined,
      key: typeof j?.key === "string" && j.key ? j.key : undefined,
      mtimeMs,
    };
  } catch {
    return { mtimeMs: 0 };
  }
}

function readStore(path: string): StoreFile | null {
  if (!existsSync(path)) return null;
  try {
    const j = JSON.parse(readFileSync(path, "utf8"));
    if (typeof j?.enc !== "string" || typeof j?.fingerprint !== "string") return null;
    return {
      url: typeof j.url === "string" ? j.url : DEFAULT_URL,
      enc: j.enc,
      imported_from_mtime_ms: Number(j.imported_from_mtime_ms) || 0,
      fingerprint: j.fingerprint,
    };
  } catch {
    return null;
  }
}

function writeAtomic(path: string, body: string) {
  mkdirSync(dirname(path), { recursive: true });
  const tmp = `${path}.${process.pid}.tmp`;
  writeFileSync(tmp, body, { mode: 0o600 });
  renameSync(tmp, path);
}

const trimUrl = (u: string) => u.replace(/\/+$/, "");

/**
 * Where the account key comes from: $C0MPUTE_API_KEY, then the keychain
 * store, then chat.json. Imports chat.json into the keychain store when the
 * store is empty or unreadable, or when chat.json was rewritten after the last
 * import with a different key (`c0mpute login` again as someone else). Never
 * throws: without a keyring it falls back to reading chat.json.
 */
export function readCreds(path = chatJsonPath(), env: NodeJS.ProcessEnv = process.env, opts: CredsOptions = {}): Creds {
  const backend = opts.backend === undefined ? defaultBackend : opts.backend;
  const storePath = opts.store ?? accountStorePath(path);
  const keychain = available(backend);
  const saved = readChatJson(path);
  const fromChatJson = (): Creds => {
    const url = trimUrl(env.C0MPUTE_URL || saved.url || DEFAULT_URL);
    if (env.C0MPUTE_API_KEY) return { url, key: env.C0MPUTE_API_KEY, source: "env", keychain };
    if (saved.key) return { url, key: saved.key, source: "chat.json", keychain };
    return { url, source: "none", keychain };
  };
  if (!keychain) return fromChatJson();

  let store = readStore(storePath);
  let key: string | undefined;
  if (store) {
    try {
      key = backend.decryptString(Buffer.from(store.enc, "base64")) || undefined;
    } catch {
      key = undefined;
    }
    if (!key) store = null; // corrupt or from another keyring: import again
  }

  const changed = saved.key && (!store || (saved.mtimeMs > store.imported_from_mtime_ms && keyFingerprint(saved.key) !== store.fingerprint));
  if (saved.key && changed) {
    try {
      const next: StoreFile = {
        url: trimUrl(saved.url || DEFAULT_URL),
        enc: backend.encryptString(saved.key).toString("base64"),
        imported_from_mtime_ms: saved.mtimeMs,
        fingerprint: keyFingerprint(saved.key),
      };
      writeAtomic(storePath, JSON.stringify(next, null, 2));
      store = next;
      key = saved.key;
    } catch {
      return fromChatJson(); // could not encrypt or write: chat.json as before
    }
  }

  const url = trimUrl(env.C0MPUTE_URL || store?.url || saved.url || DEFAULT_URL);
  if (env.C0MPUTE_API_KEY) return { url, key: env.C0MPUTE_API_KEY, source: "env", keychain };
  if (key) return { url, key, source: "keychain", keychain };
  return { url, source: "none", keychain };
}

/** Remove the desktop's keychain copy of the account key. */
export function clearKeychainCreds(path = chatJsonPath(), opts: Pick<CredsOptions, "store"> = {}) {
  rmSync(opts.store ?? accountStorePath(path), { force: true });
}

/**
 * Drop the key from chat.json, keeping the rest (the URL). Sign-out runs
 * `c0mpute chat logout` first; this makes sure the key is gone even when the
 * CLI is missing or failed, so the next read cannot import it again.
 */
export function forgetChatJsonKey(path = chatJsonPath()): boolean {
  if (!existsSync(path)) return false;
  let j: Record<string, unknown>;
  try {
    j = JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return false;
  }
  if (!j || typeof j !== "object" || !("key" in j)) return false;
  delete j.key;
  writeAtomic(path, JSON.stringify(j, null, 2));
  return true;
}

export class ApiError extends Error {
  constructor(
    message: string,
    public status: number,
  ) {
    super(message);
  }
}

/** GET/POST against c0mpute.com with the account key. */
export async function api<T = unknown>(path: string, init: RequestInit & { creds?: Creds; timeoutMs?: number } = {}): Promise<T> {
  const creds = init.creds ?? readCreds();
  if (!creds.key) throw new ApiError("not signed in: use Sign in (runs `c0mpute chat login`)", 401);
  const headers = new Headers(init.headers);
  headers.set("authorization", `Bearer ${creds.key}`);
  if (init.body && !headers.has("content-type")) headers.set("content-type", "application/json");
  const res = await fetch(`${creds.url}${path}`, {
    ...init,
    headers,
    signal: init.signal ?? AbortSignal.timeout(init.timeoutMs ?? 20_000),
  });
  const type = res.headers.get("content-type") ?? "";
  const body = type.includes("json") ? await res.json().catch(() => ({})) : await res.text();
  if (!res.ok && res.status !== 202) {
    const msg = (body as { error?: { message?: string } | string })?.error;
    throw new ApiError(typeof msg === "string" ? msg : (msg?.message ?? `HTTP ${res.status} from ${path}`), res.status);
  }
  return body as T;
}

export interface Me {
  id: string;
  did: string | null;
  name: string | null;
  balance: { spendable_usd: string; earned_spendable_usd: string; prepaid_available_usd: string } | null;
}

/** OpenAI-shaped completion (non-streaming) for the Agent chat. */
export async function complete(
  messages: { role: string; content: string }[],
  model: string | undefined,
  signal?: AbortSignal,
): Promise<{ content: string; message?: unknown; model?: string }> {
  const body = await api<{ model?: string; choices?: { message?: { content?: string } }[] }>("/v1/chat/completions", {
    method: "POST",
    body: JSON.stringify({ model: model || undefined, messages, stream: false, max_tokens: 4096 }),
    // The server no longer caps a slow node (it streams underneath); 300 s here
    // cut long answers off with "The operation timed out." (2026-11-28).
    // 20 min matches the server's own stop for a turn.
    signal: signal ? AbortSignal.any([signal, AbortSignal.timeout(20 * 60_000)]) : AbortSignal.timeout(20 * 60_000),
  });
  const message = body.choices?.[0]?.message;
  return { content: message?.content ?? "", message, model: body.model };
}

export async function models(): Promise<string[]> {
  const creds = readCreds();
  const res = await fetch(`${creds.url}/v1/models`, {
    headers: creds.key ? { authorization: `Bearer ${creds.key}` } : {},
    signal: AbortSignal.timeout(15_000),
  });
  if (!res.ok) throw new Error(`models: HTTP ${res.status}`);
  const body = (await res.json()) as { data?: { id: string }[] };
  return (body.data ?? []).map((m) => m.id);
}
