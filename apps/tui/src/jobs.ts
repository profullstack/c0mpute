/**
 * Generation jobs: /image, /video, /music, /album and /promo.
 *
 * Each one is started with the same POST the web chat makes and then polled
 * at its `poll` URL, which relays the network's live progress (the serving
 * node's /v1/progress: stage, worker, queue position, ETA). Finished results
 * are saved to the downloads dir. Jobs persist in ~/.c0mpute/tui-jobs.json,
 * so quitting and coming back picks the polling up where it was.
 *
 * The state transitions are pure functions of a poll response (tested); the
 * runner below does the I/O.
 */
import { existsSync } from "node:fs";
import { join } from "node:path";
import { Api, ApiError, errorMessage } from "./api.ts";
import { describeAlbumStage, sanitizeAlbum, type AlbumView } from "./web/album.ts";
import { audioExtension, describeNodeProgress, sanitizeNodeProgress } from "./web/music.ts";
import { describePromoProgress, sanitizePromoProgress, STAGES as PROMO_STAGES } from "./web/promo.ts";

export type JobKind = "image" | "video" | "music" | "album" | "promo";
export type JobStatus = "starting" | "pending" | "saving" | "done" | "failed" | "stopped";

export interface SavedFile {
  path: string;
  type: string;
  label: string;
}

export interface Job {
  /** Server id (local id until the POST answers). */
  id: string;
  kind: JobKind;
  /** What was asked, shown in lists. */
  prompt: string;
  /** The POST body, for retry. */
  request: Record<string, unknown>;
  poll: string | null;
  status: JobStatus;
  /** One-line state: "queued (#2 in line)", "rendering on node …ab12cd34". */
  stage: string;
  /** Extra lines: promo stages, album tracks. */
  lines: string[];
  worker: string | null;
  /** 0..1 when the network gives enough to say, else null. */
  progress: number | null;
  /** Seconds left, the serving node's estimate. */
  etaS: number | null;
  queuePosition: number | null;
  title: string | null;
  error: string | null;
  createdAt: number;
  updatedAt: number;
  finishedAt: number | null;
  files: SavedFile[];
  adult: boolean;
  /** Song or album: the lyrics the network wrote, kept for the chat. */
  lyrics: string | null;
}

export const KIND_LABEL: Record<JobKind, string> = {
  image: "image",
  video: "video",
  music: "song",
  album: "album",
  promo: "promo",
};

/** Where each kind starts. */
export const START_PATH: Record<JobKind, string> = {
  image: "/api/v1/images",
  video: "/api/v1/videos",
  music: "/api/v1/music",
  album: "/api/v1/album",
  promo: "/api/v1/promo",
};

/** How often to poll: the web chat's own cadence per kind. */
export const POLL_MS: Record<JobKind, number> = { image: 4000, video: 10_000, music: 5000, album: 5000, promo: 4000 };

/** The network queues a job up to 2 h; albums take a while longer. */
export const DEADLINE_MS: Record<JobKind, number> = {
  image: 2 * 3600_000,
  video: 3 * 3600_000,
  music: 2 * 3600_000,
  album: 4 * 3600_000,
  promo: 2 * 3600_000,
};

export function newJob(kind: JobKind, prompt: string, request: Record<string, unknown>, now: number): Job {
  return {
    id: `local-${now.toString(36)}`,
    kind,
    prompt,
    request,
    poll: null,
    status: "starting",
    stage: "starting…",
    lines: [],
    worker: null,
    progress: null,
    etaS: null,
    queuePosition: null,
    title: null,
    error: null,
    createdAt: now,
    updatedAt: now,
    finishedAt: null,
    files: [],
    adult: request.adult === true,
    lyrics: null,
  };
}

const num = (v: unknown) => (typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : null);
const str = (v: unknown, max = 200) => (typeof v === "string" && v.trim() ? v.slice(0, max) : null);

export const mmss = (s: number) => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, "0")}`;

function queuedLine(what: string, pos: number | null): string {
  return `queued for a c0mpute ${what} worker${pos ? ` (#${pos} in line)` : ""}; it starts when one is free`;
}

/** A 202 from the job's poll URL. */
export function applyPending(job: Job, body: Record<string, unknown> | null, now: number): Job {
  const b = body ?? {};
  const next: Job = { ...job, status: "pending", updatedAt: now };
  switch (job.kind) {
    case "image": {
      const pos = num(b.queue_position);
      next.queuePosition = pos;
      next.stage = b.stage === "queued" ? queuedLine("image", pos) : `drawing on a c0mpute CPU worker${str(b.detail) ? ` · ${str(b.detail)}` : ""}`;
      break;
    }
    case "video": {
      const pos = num(b.queue_position);
      const eta = num(b.eta_s);
      next.queuePosition = pos;
      next.etaS = eta;
      next.stage =
        b.stage === "queued"
          ? queuedLine("video", pos)
          : `${str(b.stage, 40) ?? "rendering"}${str(b.detail) ? ` · ${str(b.detail)}` : ""}`;
      const secs = num(b.seconds);
      next.progress = eta !== null && secs !== null && b.stage !== "queued" && eta + secs > 0 ? Math.min(0.99, secs / (secs + eta)) : null;
      break;
    }
    case "music": {
      const node = sanitizeNodeProgress(b.node);
      if (typeof b.title === "string") next.title = b.title.slice(0, 120);
      if (typeof b.lyrics === "string" && b.lyrics.trim()) next.lyrics = b.lyrics.slice(0, 8000);
      next.worker = node?.worker ?? null;
      next.queuePosition = node?.queue_position ?? null;
      if (b.stage === "writing") {
        next.stage = "writing lyrics and a style from your brief";
        next.progress = null;
      } else if (node) {
        next.stage = describeNodeProgress(node);
        if (node.stage === "rendering" && node.eta_s) {
          const left = Math.max(0, node.eta_s - (node.stage_elapsed_s ?? 0));
          next.etaS = left;
          next.progress = Math.min(0.99, (node.stage_elapsed_s ?? 0) / node.eta_s);
        } else {
          next.etaS = null;
          next.progress = null;
        }
      } else {
        next.stage = "composing with ACE-Step 1.5 on a c0mpute worker";
      }
      break;
    }
    case "album": {
      const view = sanitizeAlbum({ ...b, cover: b.cover ? "ready" : null });
      applyAlbum(next, view);
      next.stage = describeAlbumStage(view);
      break;
    }
    case "promo": {
      const p = sanitizePromoProgress(job.id, b);
      next.title = p.title ?? null;
      next.lines = describePromoProgress(p).split("\n");
      const done = PROMO_STAGES.filter((k) => ["done", "skipped"].includes(p.stages[k]?.state ?? "")).length;
      next.progress = done / PROMO_STAGES.length;
      const running = PROMO_STAGES.find((k) => p.stages[k]?.state === "running");
      next.stage = running ? `${running}${p.stages[running]?.detail ? `: ${p.stages[running]!.detail}` : ""}` : "starting the promo";
      break;
    }
  }
  return next;
}

function applyAlbum(job: Job, view: AlbumView): void {
  job.title = view.album ? `"${view.album}" by ${view.band ?? "?"}${view.genre ? ` · ${view.genre}` : ""}` : null;
  const total = view.tracks.length || view.tracksWanted || 0;
  const done = view.tracks.filter((t) => t.status === "done").length;
  job.progress = total ? done / total : null;
  job.lines = view.tracks.map((t) => {
    const mark = t.status === "done" ? "[x]" : t.status === "failed" ? "[!]" : t.status === "pending" ? "[~]" : "[ ]";
    const where = t.status === "pending" && t.node ? ` · ${describeNodeProgress(t.node)}` : t.error ? ` · ${t.error}` : "";
    return `${mark} ${String(t.n).padStart(2, "0")} ${t.title}${where}`;
  });
}

/** A file to fetch once the job is done. */
export interface ResultFile {
  url: string;
  name: string;
  label: string;
  /** Missing is fine (a cover that never came). */
  optional?: boolean;
}

export function slug(text: string, max = 40): string {
  const s = String(text ?? "")
    .normalize("NFKD")
    .replace(/[^\w\s-]/g, "")
    .trim()
    .replace(/\s+/g, "-")
    .toLowerCase()
    .slice(0, max)
    .replace(/-+$/, "");
  return s || "c0mpute";
}

/** "Band - Album": keep it readable, drop what a filesystem chokes on. */
export function safeName(text: string, max = 80): string {
  return (
    String(text ?? "")
      // eslint-disable-next-line no-control-regex -- strip control characters
      .replace(/[\u0000-\u001f\u007f<>:"/\\|?*]+/g, " ")
      .replace(/\s+/g, " ")
      .trim()
      .slice(0, max) || "c0mpute"
  );
}

export function baseName(job: Job): string {
  return `${slug(job.title ?? job.prompt)}-${job.id.slice(0, 8)}`;
}

/** A 200 JSON body (video, album, promo): what to download. */
export function applyDoneJson(job: Job, body: Record<string, unknown> | null, now: number): { job: Job; files: ResultFile[] } {
  const b = body ?? {};
  const next: Job = { ...job, status: "saving", updatedAt: now, progress: 1, etaS: null, stage: "saving the result" };
  const files: ResultFile[] = [];
  const base = baseName(job);
  if (job.kind === "video") {
    files.push({ url: str(b.video, 300) ?? `/api/v1/videos/${job.id}/video`, name: `${base}.mp4`, label: "video" });
  } else if (job.kind === "album") {
    const view = sanitizeAlbum({ ...b, cover: b.cover ? "ready" : null });
    applyAlbum(next, view);
    next.progress = 1;
    const name = safeName([view.band, view.album].filter(Boolean).join(" - ") || base);
    if (typeof b.zip === "string") files.push({ url: b.zip, name: `${name}.zip`, label: "album zip" });
    files.push({ url: `/api/v1/album/${job.id}/cover`, name: `${name}.png`, label: "cover", optional: true });
  } else if (job.kind === "promo") {
    const p = sanitizePromoProgress(job.id, b);
    next.title = p.title ?? job.title;
    next.lines = describePromoProgress(p).split("\n");
    files.push({ url: str(b.video, 300) ?? `/api/v1/promo/${job.id}/video`, name: `${base}.mp4`, label: "video" });
    files.push({ url: str(b.captions, 300) ?? `/api/v1/promo/${job.id}/captions`, name: `${base}.vtt`, label: "captions", optional: true });
  }
  return { job: next, files };
}

/** File name for a result that arrives as the poll's own 200 body (image, song). */
export function binaryResultName(job: Job, contentType: string): string {
  if (job.kind === "music") return `${safeName(job.title ?? slug(job.prompt))}-${job.id.slice(0, 8)}.${audioExtension(contentType)}`;
  return `${baseName(job)}.png`;
}

export function finish(job: Job, now: number): Job {
  const secs = Math.round((now - job.createdAt) / 1000);
  return { ...job, status: "done", progress: 1, etaS: null, finishedAt: now, updatedAt: now, stage: `done in ${mmss(secs)}` };
}

export function fail(job: Job, message: string, now: number): Job {
  return { ...job, status: "failed", error: message, finishedAt: now, updatedAt: now, stage: "failed", etaS: null };
}

/** Avoid clobbering: `name.png` → `name-2.png` when taken. */
export function uniquePath(dir: string, name: string, exists: (p: string) => boolean = existsSync): string {
  let p = join(dir, name);
  if (!exists(p)) return p;
  const dot = name.lastIndexOf(".");
  const stem = dot > 0 ? name.slice(0, dot) : name;
  const ext = dot > 0 ? name.slice(dot) : "";
  for (let i = 2; i < 1000; i++) {
    p = join(dir, `${stem}-${i}${ext}`);
    if (!exists(p)) return p;
  }
  return p;
}

/** The start POST's 202 → the job's id and poll path. */
export function applyStarted(job: Job, body: Record<string, unknown> | null, now: number): Job {
  const id = str(body?.id, 100);
  const poll = str(body?.poll, 300);
  if (!id || !poll) return fail(job, "the network did not return a job id", now);
  const extra = job.kind === "album" && typeof body?.tracks === "number" && body?.limited ? [`only ${body.tracks} free songs left today, so this album has ${body.tracks} tracks`] : [];
  return { ...job, id, poll, status: "pending", stage: "submitted; waiting for the network", lines: extra, updatedAt: now };
}

// ─── the runner ─────────────────────────────────────────────────────────

export interface JobHost {
  api: Api;
  downloadsDir: () => string;
  /** A job changed: store it, redraw, persist. */
  onChange: (job: Job, previousId: string) => void;
  now?: () => number;
}

/**
 * Polls every pending job on its own timer. Stopping a job only stops
 * watching it: the API has no cancel, so the network finishes it anyway.
 */
export class JobRunner {
  private timers = new Map<string, ReturnType<typeof setTimeout>>();
  private aborts = new Map<string, AbortController>();
  private stopped = new Set<string>();

  constructor(private readonly host: JobHost) {}

  private now(): number {
    return this.host.now?.() ?? Date.now();
  }

  private emit(job: Job, previousId = job.id): Job {
    this.host.onChange(job, previousId);
    return job;
  }

  /** POST the request and start polling. Returns the job as created. */
  async start(kind: JobKind, prompt: string, request: Record<string, unknown>): Promise<Job> {
    let job = this.emit(newJob(kind, prompt, request, this.now()));
    const localId = job.id;
    try {
      const res = await this.host.api.json(START_PATH[kind], { body: request, timeoutMs: 30_000 });
      if (res.status !== 202) {
        return this.emit(fail(job, errorMessage(res.body, res.status), this.now()));
      }
      job = applyStarted(job, res.body as Record<string, unknown>, this.now());
      this.emit(job, localId);
      if (job.status === "pending") this.schedule(job, 1500);
      return job;
    } catch (e) {
      return this.emit(fail(job, e instanceof Error ? e.message : String(e), this.now()));
    }
  }

  /** Pick a persisted pending job back up. */
  resume(job: Job): void {
    if ((job.status === "pending" || job.status === "saving") && job.poll) this.schedule(job, 500);
  }

  stop(job: Job): Job {
    this.stopped.add(job.id);
    clearTimeout(this.timers.get(job.id));
    this.aborts.get(job.id)?.abort();
    return this.emit({ ...job, status: "stopped", stage: "stopped watching (the network may still finish it)", updatedAt: this.now(), finishedAt: this.now() });
  }

  dispose(): void {
    for (const t of this.timers.values()) clearTimeout(t);
    for (const a of this.aborts.values()) a.abort();
  }

  private schedule(job: Job, ms: number): void {
    clearTimeout(this.timers.get(job.id));
    const t = setTimeout(() => void this.tick(job.id, job), ms);
    t.unref?.();
    this.timers.set(job.id, t);
  }

  private latest = new Map<string, Job>();

  /** The runner's view of a job is whatever it last emitted. */
  private async tick(id: string, seed: Job): Promise<void> {
    if (this.stopped.has(id)) return;
    let job = this.latest.get(id) ?? seed;
    if (this.now() - job.createdAt > DEADLINE_MS[job.kind]) {
      this.save(fail(job, `${KIND_LABEL[job.kind]} timed out`, this.now()));
      return;
    }
    const ctrl = new AbortController();
    this.aborts.set(id, ctrl);
    try {
      const res = await this.host.api.raw(job.poll!, { timeoutMs: 60_000, signal: ctrl.signal });
      const type = (res.headers.get("content-type") ?? "").split(";")[0]!.trim().toLowerCase();
      if (res.status === 202) {
        job = applyPending(job, (await res.json().catch(() => null)) as Record<string, unknown> | null, this.now());
        this.save(job);
        this.schedule(job, POLL_MS[job.kind]);
        return;
      }
      if (res.status === 401) {
        await res.body?.cancel().catch(() => {});
        this.save({ ...job, stage: "paused: not signed in (run `c0mpute chat login`, then restart the TUI)" });
        return;
      }
      if (res.status === 503 || res.status === 502 || res.status === 504) {
        const body = await res.json().catch(() => null);
        this.save({ ...job, stage: `the network is restarting (${errorMessage(body, res.status)}); still waiting` });
        this.schedule(job, POLL_MS[job.kind] * 3);
        return;
      }
      if (res.status !== 200) {
        const body = await res.json().catch(() => null);
        this.save(fail(job, errorMessage(body, res.status), this.now()));
        return;
      }
      // Images and songs: the poll's own 200 is the file.
      if (type.startsWith("image/") || type.startsWith("audio/")) {
        job = { ...job, status: "saving", stage: "saving the result", progress: 1 };
        this.save(job);
        const dest = uniquePath(this.host.downloadsDir(), binaryResultName(job, type));
        await saveBody(res, dest);
        const files: SavedFile[] = [{ path: dest, type, label: job.kind === "music" ? "song" : "image" }];
        if (job.kind === "music") {
          const cover = await this.fetchOptional(`${job.poll}/cover`, dest.replace(/\.[a-z0-9]+$/i, ".png"));
          if (cover) files.push({ path: cover, type: "image/png", label: "cover" });
        }
        this.save(finish({ ...job, files }, this.now()));
        return;
      }
      const body = (await res.json().catch(() => null)) as Record<string, unknown> | null;
      const { job: saving, files: wanted } = applyDoneJson(job, body, this.now());
      job = saving;
      this.save(job);
      const files: SavedFile[] = [];
      for (const f of wanted) {
        const dest = uniquePath(this.host.downloadsDir(), f.name);
        if (f.optional) {
          const got = await this.fetchOptional(f.url, dest);
          if (got) files.push({ path: got, type: "", label: f.label });
        } else {
          const r = await this.host.api.download(f.url, dest);
          files.push({ path: dest, type: r.type, label: f.label });
        }
      }
      this.save(finish({ ...job, files }, this.now()));
    } catch (e) {
      if (this.stopped.has(id)) return;
      if (e instanceof ApiError && e.kind === "offline") {
        this.save({ ...job, stage: `${e.message}; retrying` });
        this.schedule(job, Math.max(15_000, POLL_MS[job.kind] * 3));
        return;
      }
      this.save(fail(job, e instanceof Error ? e.message : String(e), this.now()));
    } finally {
      this.aborts.delete(id);
    }
  }

  private save(job: Job): void {
    this.latest.set(job.id, job);
    this.emit(job);
  }

  private async fetchOptional(url: string, dest: string): Promise<string | null> {
    try {
      await this.host.api.download(url, dest);
      return dest;
    } catch {
      return null;
    }
  }
}

async function saveBody(res: Response, dest: string): Promise<void> {
  const { mkdirSync, createWriteStream, renameSync } = await import("node:fs");
  const { dirname } = await import("node:path");
  const { Readable } = await import("node:stream");
  const { pipeline } = await import("node:stream/promises");
  mkdirSync(dirname(dest), { recursive: true });
  const part = `${dest}.part`;
  await pipeline(Readable.fromWeb(res.body as unknown as import("node:stream/web").ReadableStream), createWriteStream(part));
  renameSync(part, dest);
}

/** The body the start POST sends, per kind (the web chat's exact shapes). */
export function startBody(
  kind: JobKind,
  args: { prompt: string; lyrics?: string; adult?: boolean; model?: string; style?: string; vertical?: boolean; brief?: string; url?: string },
): Record<string, unknown> {
  switch (kind) {
    case "image":
      return args.adult ? { prompt: args.prompt, adult: true } : { prompt: args.prompt };
    case "video":
      return { prompt: args.prompt, model: args.model, ...(args.adult ? { adult: true } : {}) };
    case "music":
      return { prompt: args.prompt, lyrics: args.lyrics ?? "" };
    case "album":
      return { prompt: args.prompt };
    case "promo":
      return { url: args.url, style: args.style, vertical: args.vertical, brief: args.brief };
  }
}
