/**
 * c0mpute.com's chat API, the same endpoints the web chat and `c0mpute chat`
 * use: /api/v1/{me,models,balance,chat/completions,images,videos,music,album,
 * promo,hosting/...}, signed in with the `c0mpute chat login` key (Bearer).
 *
 * Nothing here blocks the UI: every call is async with a timeout, and every
 * failure becomes an ApiError whose message says what to do about it.
 */
import { createWriteStream, mkdirSync, renameSync, unlinkSync } from "node:fs";
import { dirname } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import type { Creds } from "./config.ts";

export type ApiErrorKind = "offline" | "auth" | "http" | "aborted";

/** A conversation as the account stores it (see the web's convo-store). */
export type SyncConvo = { id: string; title: string; model: string; updated: number; messages: ({ role: string; content: string } & Record<string, unknown>)[] };
export type SyncSummary = { id: string; title: string; model: string; updated: number; deleted: boolean };

export class ApiError extends Error {
  constructor(
    message: string,
    readonly kind: ApiErrorKind,
    readonly status = 0,
  ) {
    super(message);
  }
}

export const LOGIN_HINT = "not signed in: run `c0mpute chat login` (or press L in Settings)";

/** `error.message` (OpenAI envelope), `error` (string) or the status. */
export function errorMessage(body: unknown, status: number): string {
  const b = body as { error?: { message?: unknown } | string; message?: unknown } | null;
  if (b && typeof b.error === "object" && b.error && typeof b.error.message === "string") return b.error.message;
  if (b && typeof b.error === "string") return b.error;
  if (b && typeof b.message === "string") return b.message;
  return `HTTP ${status}`;
}

export interface ApiResponse<T = unknown> {
  status: number;
  body: T | null;
  headers: Headers;
}

export type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;

export class Api {
  constructor(
    public creds: Creds,
    private readonly fetchImpl: FetchLike = (i, o) => fetch(i, o),
  ) {}

  get signedIn(): boolean {
    return !!this.creds.key;
  }

  url(path: string): string {
    return /^https?:\/\//.test(path) ? path : `${this.creds.url}${path}`;
  }

  private headers(auth: boolean, extra: Record<string, string> = {}): Record<string, string> {
    const h: Record<string, string> = { "user-agent": "c0mpute-tui", ...extra };
    if (auth && this.creds.key) h.authorization = `Bearer ${this.creds.key}`;
    return h;
  }

  async raw(path: string, init: RequestInit & { auth?: boolean; timeoutMs?: number } = {}): Promise<Response> {
    const { auth = true, timeoutMs = 20_000, signal, ...rest } = init;
    if (auth && !this.creds.key) throw new ApiError(LOGIN_HINT, "auth", 401);
    const timeout = AbortSignal.timeout(timeoutMs);
    const merged = signal ? AbortSignal.any([signal, timeout]) : timeout;
    try {
      return await this.fetchImpl(this.url(path), {
        ...rest,
        headers: this.headers(auth, (rest.headers as Record<string, string>) ?? {}),
        signal: merged,
      });
    } catch (e) {
      if (signal?.aborted) throw new ApiError("stopped", "aborted");
      const host = new URL(this.url(path)).host;
      const why = timeout.aborted ? "timed out" : e instanceof Error ? e.message : String(e);
      throw new ApiError(`offline: could not reach ${host} (${why})`, "offline");
    }
  }

  /** JSON request. Throws on 401 (with the login hint) and on network failure; other statuses return. */
  async json<T = Record<string, unknown>>(
    path: string,
    init: { method?: string; body?: unknown; auth?: boolean; timeoutMs?: number; signal?: AbortSignal } = {},
  ): Promise<ApiResponse<T>> {
    const res = await this.raw(path, {
      method: init.method ?? (init.body === undefined ? "GET" : "POST"),
      body: init.body === undefined ? undefined : JSON.stringify(init.body),
      headers: init.body === undefined ? {} : { "content-type": "application/json" },
      auth: init.auth,
      timeoutMs: init.timeoutMs,
      signal: init.signal,
    });
    const body = (await res.json().catch(() => null)) as T | null;
    if (res.status === 401 && init.auth !== false) {
      throw new ApiError(`${errorMessage(body, 401)} · run \`c0mpute chat login\``, "auth", 401);
    }
    return { status: res.status, body, headers: res.headers };
  }

  /**
   * POST /api/v1/chat/completions with stream: true, feeding each delta to
   * `onDelta`. Returns the whole answer.
   */
  /**
   * One answer, streamed. The turn is resumable: the server finishes it even
   * if the connection drops, and a cut-off stream reattaches by run id and
   * replays (onReset clears what was shown so the replay can redraw it).
   */
  async streamChat(
    messages: { role: string; content: string }[],
    model: string | null,
    onDelta: (text: string) => void,
    signal?: AbortSignal,
    opts: { onRun?: (id: string) => void; onReset?: () => void } = {},
  ): Promise<string> {
    const res = await this.raw("/api/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ ...(model ? { model } : {}), messages, stream: true, resumable: true }),
      timeoutMs: 15 * 60_000,
      signal,
    });
    if (!res.ok) {
      const body = await res.json().catch(() => null);
      if (res.status === 401) throw new ApiError(`${errorMessage(body, 401)} · run \`c0mpute chat login\``, "auth", 401);
      throw new ApiError(errorMessage(body, res.status), "http", res.status);
    }
    const run = res.headers.get("x-c0mpute-run");
    if (run) opts.onRun?.(run);
    try {
      return await this.readAnswer(res, onDelta, signal);
    } catch (e) {
      if (run && e instanceof ApiError && e.kind === "offline") return this.resumeRun(run, onDelta, signal, opts.onReset);
      throw e;
    }
  }

  /** Reattach to a resumable turn (a few tries with backoff); the replay starts at the beginning. */
  async resumeRun(run: string, onDelta: (text: string) => void, signal?: AbortSignal, onReset?: () => void, tries = 6): Promise<string> {
    let last: unknown = null;
    for (let attempt = 0; attempt < tries; attempt++) {
      if (attempt) await new Promise((r) => setTimeout(r, Math.min(8000, 250 * 2 ** attempt)));
      if (signal?.aborted) throw new ApiError("stopped", "aborted");
      let res: Response;
      try {
        res = await this.raw(`/api/v1/chat/runs/${encodeURIComponent(run)}`, { timeoutMs: 15 * 60_000, signal });
      } catch (e) {
        last = e;
        if (e instanceof ApiError && (e.kind === "aborted" || e.kind === "auth")) throw e;
        continue;
      }
      if (res.status === 404) throw new ApiError("the connection dropped and the server no longer has this answer; send it again", "http", 404);
      if (!res.ok) {
        last = new ApiError(errorMessage(await res.json().catch(() => null), res.status), "http", res.status);
        continue;
      }
      onReset?.();
      try {
        return await this.readAnswer(res, onDelta, signal);
      } catch (e) {
        last = e;
        if (!(e instanceof ApiError && e.kind === "offline")) throw e;
      }
    }
    throw last instanceof ApiError ? last : new ApiError("lost the connection to this answer", "offline");
  }

  /** Stop a resumable turn (closing its connection does not). Best effort. */
  async cancelRun(run: string): Promise<void> {
    await this.raw(`/api/v1/chat/runs/${encodeURIComponent(run)}`, { method: "DELETE", timeoutMs: 10_000 }).catch(() => {});
  }

  // ─── conversations synced to the account ───────────────────────────

  async listConversations(since = 1): Promise<SyncSummary[]> {
    const res = await this.raw(`/api/v1/chat/conversations?since=${since}`, { timeoutMs: 20_000 });
    if (!res.ok) throw new ApiError(errorMessage(await res.json().catch(() => null), res.status), "http", res.status);
    return ((await res.json()) as { data?: SyncSummary[] }).data ?? [];
  }

  async getConversation(id: string): Promise<SyncConvo | null> {
    const res = await this.raw(`/api/v1/chat/conversations/${encodeURIComponent(id)}`, { timeoutMs: 20_000 });
    if (res.status === 404) return null;
    if (!res.ok) throw new ApiError(errorMessage(await res.json().catch(() => null), res.status), "http", res.status);
    return (await res.json()) as SyncConvo;
  }

  /** Save; on 409 returns the newer server copy instead. */
  async putConversation(c: SyncConvo): Promise<{ ok: true } | { ok: false; newer: SyncConvo | null }> {
    const res = await this.raw(`/api/v1/chat/conversations/${encodeURIComponent(c.id)}`, {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(c),
      timeoutMs: 20_000,
    });
    if (res.status === 409) {
      const body = (await res.json().catch(() => ({}))) as { conversation?: SyncConvo };
      return { ok: false, newer: body.conversation ?? null };
    }
    if (!res.ok) throw new ApiError(errorMessage(await res.json().catch(() => null), res.status), "http", res.status);
    return { ok: true };
  }

  private async readAnswer(res: Response, onDelta: (text: string) => void, signal?: AbortSignal): Promise<string> {
    if (!res.body) throw new ApiError("empty answer stream", "http", res.status);
    let answer = "";
    const parser = sseParser((data) => {
      if (data === "[DONE]") return true;
      let v: { error?: { message?: string }; choices?: { delta?: { content?: string } }[] };
      try {
        v = JSON.parse(data);
      } catch {
        return false;
      }
      if (v.error?.message) throw new ApiError(v.error.message, "http", 502);
      const t = v.choices?.[0]?.delta?.content;
      if (t) {
        answer += t;
        onDelta(t);
      }
      return false;
    });
    const decoder = new TextDecoder();
    try {
      for await (const chunk of res.body as unknown as AsyncIterable<Uint8Array>) {
        if (parser(decoder.decode(chunk, { stream: true }))) break;
      }
    } catch (e) {
      if (e instanceof ApiError) throw e;
      if (signal?.aborted) throw new ApiError("stopped", "aborted");
      throw new ApiError("the answer stream was cut off (the serving node or the network restarted); try again", "offline");
    }
    return answer;
  }

  /** Stream a result to `dest` (via a .part file). Returns the content type. */
  async download(path: string, dest: string, signal?: AbortSignal): Promise<{ type: string; bytes: number }> {
    const res = await this.raw(path, { timeoutMs: 10 * 60_000, signal });
    if (!res.ok || !res.body) {
      const body = await res.json().catch(() => null);
      throw new ApiError(errorMessage(body, res.status), res.status === 401 ? "auth" : "http", res.status);
    }
    mkdirSync(dirname(dest), { recursive: true });
    const part = `${dest}.part`;
    let bytes = 0;
    const counted = Readable.fromWeb(res.body as unknown as import("node:stream/web").ReadableStream).on("data", (c: Buffer) => {
      bytes += c.length;
    });
    try {
      await pipeline(counted, createWriteStream(part));
      renameSync(part, dest);
    } catch (e) {
      try {
        unlinkSync(part);
      } catch {}
      throw e;
    }
    return { type: (res.headers.get("content-type") ?? "").split(";")[0]!.trim(), bytes };
  }
}

/**
 * Server-sent events, fed in arbitrary chunks. Calls `onData` with each
 * `data:` payload; when it returns true the stream is finished and the
 * parser reports true from then on.
 */
export function sseParser(onData: (data: string) => boolean): (chunk: string) => boolean {
  let buf = "";
  let done = false;
  return (chunk: string) => {
    if (done) return true;
    buf += chunk;
    let nl: number;
    while ((nl = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, nl).trim();
      buf = buf.slice(nl + 1);
      if (!line.startsWith("data:")) continue;
      if (onData(line.slice(5).trim())) {
        done = true;
        return true;
      }
    }
    return false;
  };
}
