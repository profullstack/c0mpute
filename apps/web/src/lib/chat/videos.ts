// Video generation on c0mpute.com: POST /api/v1/videos. The clip renders on a
// c0mpute worker (video.txt2vid, Wan 2.1 1.3B on CPU by default) behind
// Infernet's /api/chat/video, which owns billing and the free tier; c0mpute.com
// checks the content policy, forwards, and can hold the request open
// (`Prefer: wait=N`) until the MP4 is ready.

export const VIDEO_MODELS = ["wan2.1-t2v-1.3b", "wan2.2-ti2v-5b"] as const;
export const DEFAULT_VIDEO_MODEL = "wan2.1-t2v-1.3b";
/** The network queues a clip for a worker up to 2 h; so may a caller wait. */
export const MAX_WAIT_S = 7200;

export type VideoRequest = { prompt: string; model: string; negative_prompt?: string; seed?: number; adult?: true };

/** Validate a POST body; returns the body to forward or an error string. Text to video only. */
export function parseVideoRequest(body: unknown): VideoRequest | string {
  const b = (body ?? {}) as Record<string, unknown>;
  const prompt = typeof b.prompt === "string" ? b.prompt.trim() : "";
  if (!prompt || prompt.length > 2000) return "prompt must contain 1 to 2000 characters";
  const model = b.model === undefined ? DEFAULT_VIDEO_MODEL : b.model;
  if (typeof model !== "string" || !(VIDEO_MODELS as readonly string[]).includes(model)) return `model must be one of: ${VIDEO_MODELS.join(", ")}`;
  if (b.image !== undefined || b.image_base64 !== undefined || b.image_url !== undefined) return "image inputs are not accepted; send a text prompt";
  const out: VideoRequest = { prompt, model };
  if (typeof b.negative_prompt === "string" && b.negative_prompt.trim()) out.negative_prompt = b.negative_prompt.trim().slice(0, 2000);
  if (b.seed !== undefined && b.seed !== null) {
    const seed = Number(b.seed);
    if (!Number.isInteger(seed) || seed < 0 || seed > 4294967295) return "seed must be an integer from 0 to 4294967295";
    out.seed = seed;
  }
  if (b.adult === true) out.adult = true;
  return out;
}

/**
 * `Prefer: wait=N` -> seconds to hold the request open, clamped to
 * [0, min(MAX_WAIT_S, cap)]. No header: 0 (answer 202 at once). `cap` is how
 * long the proxy in front of this app holds a request (C0MPUTE_VIDEO_MAX_HOLD_S).
 */
export function preferWait(header: string | null, cap = MAX_WAIT_S): number {
  const m = /(?:^|[,;\s])wait\s*=\s*(\d+)/i.exec(header ?? "");
  if (!m) return 0;
  return Math.max(0, Math.min(Number(m[1]), MAX_WAIT_S, Math.max(0, cap)));
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
export const isVideoJobId = (id: string) => UUID.test(id);

// ── the /video chat command (web chat, `c0mpute chat` and the TUI) ──────────

export type VideoCommand = { prompt: string; model: string };

/**
 * `/video a paper boat on a pond` (also /clip, /vid): text to video on CPU
 * workers via POST /api/v1/videos. `--model wan2.2-ti2v-5b` picks the GPU model.
 * Returns null when the line is not a video command, a string (usage or
 * error) when it is one but malformed.
 */
export function parseVideoCommand(text: string): VideoCommand | string | null {
  const m = /^\/(?:video|clip|vid)(?:\s+([\s\S]*))?$/i.exec(String(text ?? "").trim());
  if (!m) return null;
  let rest = (m[1] ?? "").trim();
  let model: string = DEFAULT_VIDEO_MODEL;
  const flag = /(?:^|\s)--model[=\s]+(\S+)/.exec(rest);
  if (flag) {
    if (!(VIDEO_MODELS as readonly string[]).includes(flag[1]!)) return `model must be one of: ${VIDEO_MODELS.join(", ")}`;
    model = flag[1]!;
    rest = rest.replace(flag[0], " ").trim();
  }
  if (!rest) return "usage: /video <what happens in the clip>";
  if (rest.length > 2000) return "prompt must contain 1 to 2000 characters";
  return { prompt: rest, model };
}

/**
 * The POST /api/v1/videos body for a command. `adult` is the chat's 18+ mode
 * (attested account AND the switch on), exactly as /image sends it: the key
 * is present only when true, never `adult: false`.
 */
export function videoRequestBody(cmd: VideoCommand, adult: boolean): { prompt: string; model: string; adult?: true } {
  return adult ? { prompt: cmd.prompt, model: cmd.model, adult: true } : { prompt: cmd.prompt, model: cmd.model };
}

export type VideoProgress = { stage?: string | null; detail?: string | null; queue_position?: number | null; eta_s?: number | null; worker?: string | null };

const mins = (s: number) => (s < 90 ? `${Math.max(1, Math.round(s))}s` : `${Math.round(s / 60)} min`);

/** One line for a pending clip: where it is, its place in line and the ETA. */
export function describeVideoProgress(p: VideoProgress): string {
  const stage = (p.stage ?? "").trim();
  const parts: string[] = [];
  if (!stage || stage === "queued" || stage === "pending") {
    parts.push(`queued for a c0mpute worker${typeof p.queue_position === "number" ? ` (#${p.queue_position} in line)` : ""}, it starts when one is free`);
  } else {
    parts.push(stage);
  }
  if (p.detail && p.detail.trim() && p.detail.trim() !== stage) parts.push(p.detail.trim());
  if (p.worker) parts.push(`worker ${p.worker}`);
  if (typeof p.eta_s === "number" && p.eta_s > 0) parts.push(`about ${mins(p.eta_s)} left`);
  return parts.join(" · ");
}

export type VideoPoll = { kind: "pending" } | { kind: "done" } | { kind: "retry" } | { kind: "failed"; message: string };

/**
 * What one GET /api/v1/videos/<id> answer means for a chat card. A deploy of
 * c0mpute.com or the network answers 502/503 with an HTML page (no JSON) or
 * `error.retry: true` for a minute: the job is not lost, keep polling. A JSON
 * error without `retry` is the job's own failure and ends the card.
 */
export function videoPollOutcome(status: number, body: unknown): VideoPoll {
  const b = (body && typeof body === "object" ? body : null) as { status?: unknown; error?: { message?: unknown; retry?: unknown } } | null;
  if (status === 202) return { kind: "pending" };
  if (status === 200 && b?.status === "done") return { kind: "done" };
  if (status === 200) return { kind: "retry" };
  if (status === 0 || status === 408 || status === 429) return { kind: "retry" };
  if (status >= 500 && (!b?.error || b.error.retry === true)) return { kind: "retry" };
  const message = typeof b?.error?.message === "string" ? b.error.message : `video failed (HTTP ${status})`;
  return { kind: "failed", message };
}
