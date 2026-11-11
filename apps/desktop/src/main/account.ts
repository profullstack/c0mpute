// The c0mpute.com account this machine is signed in to. `c0mpute chat login`
// (CoinPay device flow, DID required) writes the key to ~/.c0mpute/chat.json
// (0600); the app reads it from there and never keeps a copy of its own.
// C0MPUTE_API_KEY / C0MPUTE_URL override it, as they do for the CLI
// (node/crates/c0mpute-cli/src/ai_chat.rs).

import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export const DEFAULT_URL = "https://c0mpute.com";

export function chatJsonPath(home = process.env.HOME || homedir()): string {
  return join(home, ".c0mpute", "chat.json");
}

export interface Creds {
  url: string;
  key?: string;
  source: "env" | "chat.json" | "none";
}

export function readCreds(path = chatJsonPath(), env: NodeJS.ProcessEnv = process.env): Creds {
  let saved: { url?: string; key?: string } = {};
  if (existsSync(path)) {
    try {
      saved = JSON.parse(readFileSync(path, "utf8"));
    } catch {
      saved = {};
    }
  }
  const url = (env.C0MPUTE_URL || saved.url || DEFAULT_URL).replace(/\/+$/, "");
  if (env.C0MPUTE_API_KEY) return { url, key: env.C0MPUTE_API_KEY, source: "env" };
  if (saved.key) return { url, key: saved.key, source: "chat.json" };
  return { url, source: "none" };
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
    signal: signal ? AbortSignal.any([signal, AbortSignal.timeout(300_000)]) : AbortSignal.timeout(300_000),
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
