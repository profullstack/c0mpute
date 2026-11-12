// Resumable chat turns. A turn asked for with `resumable: true` is read to the
// end by the server whatever happens to the connection, and its SSE output is
// buffered here under a run id. A client that drops (sleep, wifi, a closed
// tab, ssh dying) reattaches with GET /api/v1/chat/runs/<id>: the replay
// starts at the beginning and then follows the turn live.
//
// Runs live in this process's memory: a deploy or restart loses the ones in
// flight (the conversation itself is safe in the account's synced history).

const enc = new TextEncoder();

export const RUN_TTL_MS = 15 * 60_000;
export const RUN_MAX_BYTES = 2 * 1024 * 1024;
/** Turns one account may have running at once. */
export const RUN_MAX_ACTIVE = 6;
/** A turn that never ends is cut off after this. */
export const RUN_MAX_MS = 20 * 60_000;

export type RunStatus = "running" | "done" | "failed" | "cancelled";

type Listener = (chunk: Uint8Array | null) => void;

export class Run {
  status: RunStatus = "running";
  finished: number | null = null;
  readonly chunks: Uint8Array[] = [];
  bytes = 0;
  readonly listeners = new Set<Listener>();
  readonly abort = new AbortController();

  constructor(
    readonly id: string,
    readonly userId: string,
    readonly started: number,
  ) {}

  push(chunk: Uint8Array) {
    if (this.status !== "running") return;
    if (this.bytes + chunk.byteLength > RUN_MAX_BYTES) {
      this.push(enc.encode(`data: ${JSON.stringify({ error: { message: "the answer passed the 2 MiB resumable-turn cap and was cut off", type: "c0mpute_error", code: 413 } })}\n\ndata: [DONE]\n\n`).slice(0, Math.max(0, RUN_MAX_BYTES - this.bytes)));
      this.end("failed");
      this.abort.abort();
      return;
    }
    this.chunks.push(chunk);
    this.bytes += chunk.byteLength;
    for (const l of this.listeners) l(chunk);
  }

  end(status: Exclude<RunStatus, "running">) {
    if (this.status !== "running") return;
    this.status = status;
    this.finished = Date.now();
    for (const l of this.listeners) l(null);
    this.listeners.clear();
  }

  summary() {
    return { id: this.id, status: this.status, started: this.started, finished: this.finished };
  }
}

type Registry = { runs: Map<string, Run> };
// One registry per process, also across Next's module reloads in dev.
const g = globalThis as unknown as { __c0RunRegistry?: Registry };
const reg: Registry = (g.__c0RunRegistry ??= { runs: new Map() });

function sweep(now = Date.now()) {
  for (const [id, r] of reg.runs) {
    if (r.finished !== null && now - r.finished > RUN_TTL_MS) reg.runs.delete(id);
    else if (r.status === "running" && now - r.started > RUN_MAX_MS) {
      r.abort.abort();
      r.end("failed");
    }
  }
}

export class RunLimitError extends Error {}

/**
 * Start buffering a turn. `open` gets the run's abort signal (Stop, the cap,
 * the time limit) and returns the SSE body to read; the run reads it to the
 * end on its own, so the client connection no longer matters.
 */
export async function startRun(userId: string, open: (signal: AbortSignal) => Promise<ReadableStream<Uint8Array> | null>, now = Date.now()): Promise<Run> {
  sweep(now);
  const active = [...reg.runs.values()].filter((r) => r.userId === userId && r.status === "running").length;
  if (active >= RUN_MAX_ACTIVE) throw new RunLimitError(`at most ${RUN_MAX_ACTIVE} answers running at once on one account`);
  const id = `run_${crypto.randomUUID().replace(/-/g, "")}`;
  const run = new Run(id, userId, now);
  reg.runs.set(id, run);
  run.push(enc.encode(`data: ${JSON.stringify({ object: "c0mpute.run", id })}\n\n`));
  const body = await open(run.abort.signal).catch((e) => {
    run.push(enc.encode(`data: ${JSON.stringify({ error: { message: e instanceof Error ? e.message : String(e), type: "c0mpute_error", code: 502 } })}\n\ndata: [DONE]\n\n`));
    run.end(run.abort.signal.aborted ? "cancelled" : "failed");
    return null;
  });
  if (body) void pump(run, body);
  else if (run.status === "running") run.end("failed");
  return run;
}

async function pump(run: Run, body: ReadableStream<Uint8Array>) {
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
    run.end(run.abort.signal.aborted ? "cancelled" : "done");
  } catch {
    if (run.abort.signal.aborted) {
      run.push(enc.encode("data: [DONE]\n\n"));
      run.end("cancelled");
    } else {
      run.push(enc.encode(`data: ${JSON.stringify({ error: { message: "the serving node or the network dropped the answer", type: "c0mpute_error", code: 502 } })}\n\ndata: [DONE]\n\n`));
      run.end("failed");
    }
  } finally {
    run.abort.signal.removeEventListener("abort", onAbort);
  }
}

/** The run, only for the account that started it. */
export function getRun(userId: string, id: string): Run | null {
  sweep();
  const r = reg.runs.get(id);
  return r && r.userId === userId ? r : null;
}

export function listRuns(userId: string) {
  sweep();
  return [...reg.runs.values()].filter((r) => r.userId === userId).sort((a, b) => b.started - a.started).map((r) => r.summary());
}

/** Stop a running turn (the upstream request is aborted). */
export function cancelRun(userId: string, id: string): boolean {
  const r = getRun(userId, id);
  if (!r) return false;
  if (r.status === "running") {
    r.abort.abort();
    r.push(enc.encode("data: [DONE]\n\n"));
    r.end("cancelled");
  }
  return true;
}

/** The run's output from byte `from`, then live until it ends. */
export function runStream(run: Run, from = 0): ReadableStream<Uint8Array> {
  let listener: Listener | null = null;
  return new ReadableStream<Uint8Array>({
    start(ctrl) {
      let skip = Math.max(0, from);
      for (const c of run.chunks) {
        if (skip >= c.byteLength) {
          skip -= c.byteLength;
          continue;
        }
        ctrl.enqueue(skip ? c.subarray(skip) : c);
        skip = 0;
      }
      if (run.status !== "running") {
        ctrl.close();
        return;
      }
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

export const SSE_HEADERS = { "content-type": "text/event-stream", "cache-control": "no-store", "x-accel-buffering": "no" };

/** Tests only. */
export function _resetRuns() {
  reg.runs.clear();
}
