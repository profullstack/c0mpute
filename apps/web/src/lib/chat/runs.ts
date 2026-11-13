// Resumable chat turns. A turn asked for with `resumable: true` is read to the
// end by the server whatever happens to the connection, and its SSE output is
// kept under a run id. A client that drops (sleep, wifi, a closed tab, ssh
// dying) reattaches with GET /api/v1/chat/runs/<id>: the replay starts at the
// beginning and then follows the turn live.
//
// Durable across deploys: a run's request (sealed) and its output are written
// to the database about once a second, with a heartbeat. A deploy kills the
// process that was generating an answer (and the standby container that served
// the deploy window goes away too), so on boot, and every 15 s, each server
// claims runs whose owner stopped beating and continues them: a plain answer
// picks up from the saved partial text, a plugin turn runs again. Any process
// can replay any run from the database, so a reattach that lands on another
// container still follows it.
//
// Without a database (tests, no sealing key) runs live in memory only.

import { hostname } from "node:os";
import { randomUUID } from "node:crypto";
import { seal, unseal } from "./plugin-store";
import type { Row, Sql } from "./store";

const enc = new TextEncoder();

export const RUN_TTL_MS = 15 * 60_000;
export const RUN_MAX_BYTES = 2 * 1024 * 1024;
/** Turns one account may have running at once. */
export const RUN_MAX_ACTIVE = 6;
/** A turn that never ends is cut off after this. */
export const RUN_MAX_MS = 20 * 60_000;
/** A run whose owner has not written for this long is taken over. */
export const RUN_STALE_MS = 20_000;
/** A turn whose upstream has not answered (headers) within this is failed. */
export const RUN_OPEN_MS = 5 * 60_000;
/** How many times a run is continued after its server died. */
export const RUN_MAX_ATTEMPTS = 3;

export const CONTINUE_PROMPT =
  "Your previous answer was cut off by a server restart. Continue it from exactly where it stopped: do not repeat anything already written, do not start over, and do not mention the interruption.";

export type RunStatus = "running" | "done" | "failed" | "cancelled";

/** What it takes to (re)open a turn on another process: the request as the client sent it. */
export type RunRequest = { userId: string; coinpaySub: string | null; body: Record<string, unknown>; withPlugins: boolean };

/**
 * Opens the upstream SSE for a run. `cont` is set when continuing a run whose
 * server died: `partial` is the answer text so far (empty for a plugin turn,
 * which runs again).
 */
export type RunOpener = (req: RunRequest, signal: AbortSignal, cont?: { partial: string }) => Promise<ReadableStream<Uint8Array> | null>;

export const RUN_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS chat_runs (
     id        TEXT PRIMARY KEY,
     user_id   TEXT NOT NULL,
     status    TEXT NOT NULL,
     owner     TEXT NOT NULL,
     heartbeat INTEGER NOT NULL,
     started   INTEGER NOT NULL,
     finished  INTEGER,
     attempts  INTEGER NOT NULL DEFAULT 0,
     request   TEXT
   )`,
  `CREATE INDEX IF NOT EXISTS chat_runs_user ON chat_runs (user_id, started)`,
  `CREATE INDEX IF NOT EXISTS chat_runs_status ON chat_runs (status, heartbeat)`,
  `CREATE TABLE IF NOT EXISTS chat_run_chunks (
     run_id TEXT NOT NULL,
     seq    INTEGER NOT NULL,
     data   TEXT NOT NULL,
     PRIMARY KEY (run_id, seq)
   )`,
];

const sse = (o: unknown) => `data: ${JSON.stringify(o)}\n\n`;
const errorEvent = (message: string, code = 502) => sse({ error: { message, type: "c0mpute_error", code } });

type Listener = (chunk: Uint8Array | null) => void;

/** A run this process owns: its output in memory, and what is not yet written. */
export class Run {
  status: RunStatus = "running";
  finished: number | null = null;
  readonly chunks: Uint8Array[] = [];
  bytes = 0;
  readonly listeners = new Set<Listener>();
  abort = new AbortController();
  /** chunks[0..flushed) are in the database. */
  flushed = 0;
  /** chunks[0..decoded) went through `decoder`; their text not yet written is `pending`. */
  decoded = 0;
  pending = "";
  /** seq of the next chunk row. */
  seq = 0;
  /** The status in the database differs from this one. */
  dirty = true;
  readonly decoder = new TextDecoder();

  constructor(
    readonly id: string,
    readonly userId: string,
    readonly started: number,
    readonly request: RunRequest | null,
  ) {}

  push(chunk: Uint8Array) {
    if (this.status !== "running" || !chunk.byteLength) return;
    if (this.bytes + chunk.byteLength > RUN_MAX_BYTES) {
      this.append(enc.encode(errorEvent("the answer passed the 2 MiB resumable-turn cap and was cut off", 413) + "data: [DONE]\n\n"));
      this.end("failed");
      this.abort.abort();
      return;
    }
    this.append(chunk);
  }

  private append(chunk: Uint8Array) {
    this.chunks.push(chunk);
    this.bytes += chunk.byteLength;
    for (const l of this.listeners) l(chunk);
  }

  end(status: Exclude<RunStatus, "running">, at = Date.now()) {
    if (this.status !== "running") return;
    this.status = status;
    this.finished = at;
    this.dirty = true;
    for (const l of this.listeners) l(null);
    this.listeners.clear();
  }

  summary() {
    return { id: this.id, status: this.status, started: this.started, finished: this.finished };
  }
}

export class RunLimitError extends Error {}

export type RunSummary = { id: string; status: RunStatus; started: number; finished: number | null };

/** The answer text in an SSE transcript (choices[0].delta.content). */
export function answerText(transcript: string): string {
  let out = "";
  for (const line of transcript.split("\n")) {
    const t = line.trim();
    if (!t.startsWith("data:")) continue;
    const data = t.slice(5).trim();
    if (data === "[DONE]") break;
    try {
      const j = JSON.parse(data);
      const c = j?.choices?.[0]?.delta?.content;
      if (typeof c === "string") out += c;
    } catch {}
  }
  return out;
}

export function createRunManager(opts: {
  opener: RunOpener;
  sql?: Sql | null;
  /** Seals stored requests (they hold the prompts). No key: memory only. */
  key?: Buffer | null;
  owner?: string;
  now?: () => number;
  flushMs?: number;
  /** How long the upstream may take to answer before the turn is failed. */
  openMs?: number;
  /** Start the flush/recovery timers (off in tests, which call tick()). */
  timers?: boolean;
}) {
  const owner = opts.owner ?? `${hostname()}:${process.pid}:${randomUUID().slice(0, 8)}`;
  const now = opts.now ?? Date.now;
  const db = opts.sql && opts.key ? opts.sql : null;
  const key = opts.key ?? null;
  const runs = new Map<string, Run>();
  const aad = (userId: string, id: string) => `${userId}|${id}|run`;

  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      if (!db) return;
      for (const q of RUN_SCHEMA) await db.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  function sweepMemory(t = now()) {
    for (const [id, r] of runs) {
      if (r.finished !== null && t - r.finished > RUN_TTL_MS && r.flushed === r.chunks.length && !r.dirty) runs.delete(id);
      else if (r.status === "running" && t - r.started > RUN_MAX_MS) {
        r.abort.abort();
        r.push(enc.encode(errorEvent("the answer took longer than 20 minutes and was stopped") + "data: [DONE]\n\n"));
        r.end("failed", t);
      }
    }
  }

  async function pump(run: Run, body: ReadableStream<Uint8Array>, endOnDone = true) {
    const reader = body.getReader();
    const onAbort = () => reader.cancel().catch(() => {});
    run.abort.signal.addEventListener("abort", onAbort);
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        if (value?.byteLength) run.push(value);
        if (run.status !== "running") break;
      }
      if (endOnDone) run.end(run.abort.signal.aborted ? "cancelled" : "done", now());
    } catch {
      if (run.abort.signal.aborted) {
        run.push(enc.encode("data: [DONE]\n\n"));
        run.end("cancelled", now());
      } else {
        run.push(enc.encode(errorEvent("the serving node or the network dropped the answer") + "data: [DONE]\n\n"));
        run.end("failed", now());
      }
    } finally {
      run.abort.signal.removeEventListener("abort", onAbort);
    }
    void flush();
  }

  async function open(run: Run, cont?: { partial: string }) {
    if (!run.request) return;
    // An upstream that never answers would hold the turn (and the request
    // that started it) until the 20-minute cap: fail it instead.
    const slow = setTimeout(() => {
      run.push(enc.encode(errorEvent("the c0mpute network did not answer in time; try again in a minute", 504) + "data: [DONE]\n\n"));
      run.end("failed", now());
      run.abort.abort();
    }, opts.openMs ?? RUN_OPEN_MS);
    (slow as { unref?: () => void }).unref?.();
    const body = await opts.opener(run.request, run.abort.signal, cont).catch((e) => {
      run.push(enc.encode(errorEvent(e instanceof Error ? e.message : String(e)) + "data: [DONE]\n\n"));
      run.end(run.abort.signal.aborted ? "cancelled" : "failed", now());
      return null;
    });
    clearTimeout(slow);
    if (body && run.status !== "running") void body.cancel().catch(() => {});
    else if (body) void pump(run, body);
    else if (run.status === "running") run.end("failed", now());
  }

  // ── persistence ─────────────────────────────────────────────────────

  let flushing: Promise<void> | null = null;
  /** Write new output, status changes and heartbeats; pick up cancels from other servers. */
  function flush(): Promise<void> {
    if (!db) return Promise.resolve();
    return (flushing ??= (async () => {
      try {
        await init();
        const t = now();
        for (const r of [...runs.values()]) {
          if (r.status === "running" || r.flushed < r.chunks.length || r.dirty) {
            // A cancel or a takeover from another server since the last tick.
            const [row] = await db.all(`SELECT status, owner FROM chat_runs WHERE id = ?`, [r.id]);
            if (row && String(row.owner) !== owner) {
              // Another server took this run over (we stalled): stop, it carries on.
              r.abort.abort();
              runs.delete(r.id);
              continue;
            }
            if (row && String(row.status) === "cancelled" && r.status === "running") {
              r.abort.abort();
              r.push(enc.encode("data: [DONE]\n\n"));
              r.end("cancelled", t);
            }
            // Everything new since the last tick goes in as ONE row (a round
            // trip per stream chunk made a tick outlast the heartbeat), and
            // each chunk passes the streaming decoder exactly once. `flushed`
            // and `seq` only move once the row is in: a failed write leaves
            // `pending` to go out next tick under the same seq, and OR REPLACE
            // makes a write that landed but reported an error harmless (the
            // retry holds the same text, plus whatever came since).
            const upTo = r.chunks.length;
            for (; r.decoded < upTo; r.decoded++) r.pending += r.decoder.decode(r.chunks[r.decoded]!, { stream: true });
            if (r.pending) {
              await db.run(`INSERT OR REPLACE INTO chat_run_chunks (run_id, seq, data) VALUES (?, ?, ?)`, [r.id, r.seq, r.pending]);
              r.seq++;
              r.pending = "";
            }
            r.flushed = upTo;
            // Never write over a cancel another server recorded between the
            // read above and here: the next tick sees it and stops the run.
            const status = r.status;
            await db.run(`UPDATE chat_runs SET status = ?, heartbeat = ?, finished = ? WHERE id = ? AND owner = ? AND status <> 'cancelled'`, [status, t, r.finished, r.id, owner]);
            if (r.status === status) r.dirty = false;
          }
        }
      } catch (e) {
        console.error("chat runs: flush failed", e);
      } finally {
        flushing = null;
      }
    })());
  }

  async function storedChunks(id: string, afterSeq = -1): Promise<{ seq: number; data: string }[]> {
    if (!db) return [];
    const rows = await db.all(`SELECT seq, data FROM chat_run_chunks WHERE run_id = ? AND seq > ? ORDER BY seq`, [id, afterSeq]);
    return rows.map((r) => ({ seq: Number(r.seq), data: String(r.data) }));
  }

  function openRequest(r: Row): RunRequest | null {
    if (!key || typeof r.request !== "string") return null;
    const plain = unseal(key, r.request, aad(String(r.user_id), String(r.id)));
    try {
      return plain ? (JSON.parse(plain) as RunRequest) : null;
    } catch {
      return null;
    }
  }

  /**
   * Take over runs whose server stopped beating, and continue them here.
   * Also drops finished runs older than an hour.
   */
  async function recover(): Promise<string[]> {
    if (!db) return [];
    await init();
    const t = now();
    await db.run(`DELETE FROM chat_run_chunks WHERE run_id IN (SELECT id FROM chat_runs WHERE finished IS NOT NULL AND finished < ?)`, [t - 60 * 60_000]);
    await db.run(`DELETE FROM chat_runs WHERE finished IS NOT NULL AND finished < ?`, [t - 60 * 60_000]);
    const stale = await db.all(`SELECT * FROM chat_runs WHERE status = 'running' AND heartbeat < ? AND owner <> ?`, [t - RUN_STALE_MS, owner]);
    const taken: string[] = [];
    for (const r of stale) {
      const id = String(r.id);
      const userId = String(r.user_id);
      const started = Number(r.started);
      const attempts = Number(r.attempts ?? 0);
      const req = openRequest(r);
      const giveUp = !req || attempts >= RUN_MAX_ATTEMPTS || t - started > RUN_MAX_MS;
      // Claim: only one server wins (the row must still name the dead owner).
      await db.run(`UPDATE chat_runs SET owner = ?, heartbeat = ?, attempts = attempts + 1 WHERE id = ? AND owner = ? AND status = 'running'`, [owner, t, id, String(r.owner)]);
      const [mine] = await db.all(`SELECT owner FROM chat_runs WHERE id = ?`, [id]);
      if (!mine || String(mine.owner) !== owner) continue;
      const chunks = await storedChunks(id);
      const run = new Run(id, userId, started, req);
      run.seq = chunks.length ? chunks[chunks.length - 1]!.seq + 1 : 0;
      for (const c of chunks) {
        const b = enc.encode(c.data);
        run.chunks.push(b);
        run.bytes += b.byteLength;
      }
      run.flushed = run.decoded = run.chunks.length;
      runs.set(id, run);
      taken.push(id);
      if (giveUp) {
        run.push(enc.encode(errorEvent("the server restarted while answering and the answer could not be continued; send it again") + "data: [DONE]\n\n"));
        run.end("failed", t);
        continue;
      }
      const transcript = chunks.map((c) => c.data).join("");
      await open(run, { partial: req!.withPlugins ? "" : answerText(transcript) });
    }
    if (taken.length) await flush();
    return taken;
  }

  let timers: ReturnType<typeof setInterval>[] = [];
  if (opts.timers !== false && db) {
    const t1 = setInterval(() => {
      // The 20-minute cap must not wait for someone to call start/get/list.
      sweepMemory();
      void flush();
    }, opts.flushMs ?? 1000);
    const t2 = setInterval(() => void recover().catch((e) => console.error("chat runs: recover failed", e)), 15_000);
    t1.unref?.();
    t2.unref?.();
    timers = [t1, t2];
    setTimeout(() => void recover().catch((e) => console.error("chat runs: recover failed", e)), 2000).unref?.();
  }

  return {
    owner,
    durable: !!db,

    /** Start a turn and keep it going whatever happens to the client. */
    async start(req: RunRequest): Promise<Run> {
      sweepMemory();
      const t = now();
      const localActive = [...runs.values()].filter((r) => r.userId === req.userId && r.status === "running").length;
      let active = localActive;
      if (db) {
        await init();
        const [n] = await db.all(`SELECT COUNT(*) AS n FROM chat_runs WHERE user_id = ? AND status = 'running' AND heartbeat > ?`, [req.userId, t - RUN_STALE_MS]);
        active = Math.max(localActive, Number(n?.n ?? 0));
      }
      if (active >= RUN_MAX_ACTIVE) throw new RunLimitError(`at most ${RUN_MAX_ACTIVE} answers running at once on one account`);
      const id = `run_${randomUUID().replace(/-/g, "")}`;
      const run = new Run(id, req.userId, t, req);
      // The row first: if it cannot be written the turn never existed (no
      // phantom "running" run counting against the account, no orphan chunks).
      if (db && key) {
        await db.run(`INSERT INTO chat_runs (id, user_id, status, owner, heartbeat, started, attempts, request) VALUES (?, ?, 'running', ?, ?, ?, 0, ?)`, [
          id,
          req.userId,
          owner,
          t,
          t,
          seal(key, JSON.stringify(req), aad(req.userId, id)),
        ]);
      }
      runs.set(id, run);
      run.push(enc.encode(sse({ object: "c0mpute.run", id })));
      try {
        await open(run);
      } catch (e) {
        // open() reports upstream errors into the run; anything else must not
        // leave it "running" forever.
        run.push(enc.encode(errorEvent(e instanceof Error ? e.message : String(e)) + "data: [DONE]\n\n"));
        run.end("failed", now());
      }
      return run;
    },

    /** The run's state, for its owner only (here or in the database). */
    async get(userId: string, id: string): Promise<RunSummary | null> {
      sweepMemory();
      const r = runs.get(id);
      if (r) return r.userId === userId ? r.summary() : null;
      if (!db) return null;
      await init();
      const [row] = await db.all(`SELECT id, user_id, status, started, finished FROM chat_runs WHERE id = ?`, [id]);
      if (!row || String(row.user_id) !== userId) return null;
      return { id, status: String(row.status) as RunStatus, started: Number(row.started), finished: row.finished == null ? null : Number(row.finished) };
    },

    async list(userId: string): Promise<RunSummary[]> {
      sweepMemory();
      const out = new Map<string, RunSummary>();
      if (db) {
        await init();
        const rows = await db.all(`SELECT id, status, started, finished FROM chat_runs WHERE user_id = ? AND (finished IS NULL OR finished > ?) ORDER BY started DESC LIMIT 50`, [userId, now() - RUN_TTL_MS]);
        for (const r of rows) out.set(String(r.id), { id: String(r.id), status: String(r.status) as RunStatus, started: Number(r.started), finished: r.finished == null ? null : Number(r.finished) });
      }
      for (const r of runs.values()) if (r.userId === userId) out.set(r.id, r.summary());
      return [...out.values()].sort((a, b) => b.started - a.started);
    },

    /** Stop a running turn; another server's run stops at its next tick. */
    async cancel(userId: string, id: string): Promise<boolean> {
      const r = runs.get(id);
      if (r) {
        if (r.userId !== userId) return false;
        if (r.status === "running") {
          r.abort.abort();
          r.push(enc.encode("data: [DONE]\n\n"));
          r.end("cancelled", now());
          void flush();
        }
        return true;
      }
      if (!db) return false;
      await init();
      const [row] = await db.all(`SELECT user_id, status FROM chat_runs WHERE id = ?`, [id]);
      if (!row || String(row.user_id) !== userId) return false;
      if (String(row.status) === "running") await db.run(`UPDATE chat_runs SET status = 'cancelled', finished = ? WHERE id = ? AND status = 'running'`, [now(), id]);
      return true;
    },

    /**
     * The run's output from byte `from`, then live until it ends: from memory
     * when this server owns it, else from the database (polled).
     */
    stream(id: string, from = 0, pollMs = 750): ReadableStream<Uint8Array> {
      const run = runs.get(id);
      let skip = Math.max(0, from);
      const emit = (ctrl: ReadableStreamDefaultController<Uint8Array>, c: Uint8Array) => {
        if (skip >= c.byteLength) {
          skip -= c.byteLength;
          return;
        }
        ctrl.enqueue(skip ? c.subarray(skip) : c);
        skip = 0;
      };
      if (run) {
        let listener: Listener | null = null;
        return new ReadableStream<Uint8Array>({
          start(ctrl) {
            for (const c of run.chunks) emit(ctrl, c);
            if (run.status !== "running") return ctrl.close();
            listener = (chunk) => {
              try {
                if (chunk) ctrl.enqueue(chunk);
                else ctrl.close();
              } catch {}
            };
            run.listeners.add(listener);
          },
          cancel() {
            if (listener) run.listeners.delete(listener);
          },
        });
      }
      // Another server owns it (or owned it before a restart): follow the database.
      let stopped = false;
      return new ReadableStream<Uint8Array>({
        async start(ctrl) {
          let last = -1;
          const deadline = now() + RUN_MAX_MS;
          try {
            for (;;) {
              for (const c of await storedChunks(id, last)) {
                emit(ctrl, enc.encode(c.data));
                last = c.seq;
              }
              const [row] = db ? await db.all(`SELECT status FROM chat_runs WHERE id = ?`, [id]) : [];
              const done = !row || String(row.status) !== "running";
              if (done) {
                // A cancel recorded here never reaches the owner's stream: end it cleanly.
                const tail = await storedChunks(id, last);
                for (const c of tail) emit(ctrl, enc.encode(c.data));
                if (row && String(row.status) === "cancelled" && !tail.length) ctrl.enqueue(enc.encode("data: [DONE]\n\n"));
                break;
              }
              if (stopped || now() > deadline) break;
              await new Promise((r) => setTimeout(r, pollMs));
              if (stopped) break;
            }
          } catch {}
          try {
            ctrl.close();
          } catch {}
        },
        cancel() {
          stopped = true;
        },
      });
    },

    flush,
    recover,
    /** Tests: the run this process owns. */
    local(id: string): Run | undefined {
      return runs.get(id);
    },
    /** Tests: die without a word, like a SIGKILL (no flush, no status change). */
    crash() {
      for (const t of timers) clearInterval(t);
      for (const r of runs.values()) r.abort.abort();
      runs.clear();
    },
    stop() {
      for (const t of timers) clearInterval(t);
      timers = [];
    },
  };
}

export type RunManager = ReturnType<typeof createRunManager>;

export const SSE_HEADERS = { "content-type": "text/event-stream", "cache-control": "no-store", "x-accel-buffering": "no" };
