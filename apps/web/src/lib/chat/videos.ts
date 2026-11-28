// Video generation on c0mpute.com: POST /api/v1/videos. By default a 30 s
// clip on Wan 2.2 5B (GPU workers): Infernet's /api/chat/video plans 5 s shots
// of the same subjects, renders each on a c0mpute worker (video.txt2vid) and
// cuts them together; it owns billing (per shot) and the free tier.
// c0mpute.com checks the content policy, forwards, and can hold the request
// open (`Prefer: wait=N`) until the MP4 is ready. Wan 2.1 1.3B (CPU) is a
// ~2 s draft.

/**
 * `chatrxl-uncensored` is a two-step clip (Infernet chains it): a ChatrXL
 * first frame on a CPU worker, animated by Wan 2.2 5B. SFW only, because
 * adult mode never takes an image input.
 */
export const VIDEO_MODELS = ["wan2.1-t2v-1.3b", "wan2.2-ti2v-5b", "chatrxl-uncensored"] as const;
export const KEYFRAME_VIDEO_MODELS: readonly string[] = ["chatrxl-uncensored"];
export const DEFAULT_VIDEO_MODEL = "wan2.2-ti2v-5b";
/** Clip length: 30 s by default (the standard short), 5 to 60. */
export const DEFAULT_VIDEO_SECONDS = 30;
export const MIN_VIDEO_SECONDS = 5;
export const MAX_VIDEO_SECONDS = 60;
/**
 * Adult clips on the default model render on Wan 2.2 5B (it carries the adult
 * LoRAs). C0MPUTE_ADULT_VIDEO_MODEL overrides it.
 */
export const ADULT_VIDEO_MODEL = "wan2.2-ti2v-5b";
/** The network queues a clip for a worker up to 2 h; so may a caller wait. */
export const MAX_WAIT_S = 7200;

export type VideoRequest = { prompt: string; model: string; seconds?: number; vertical?: true; negative_prompt?: string; seed?: number; adult?: true };

/** Validate a POST body; returns the body to forward or an error string. Text to video only. */
export function parseVideoRequest(body: unknown, opts: { adultModel?: string } = {}): VideoRequest | string {
  const b = (body ?? {}) as Record<string, unknown>;
  const prompt = typeof b.prompt === "string" ? b.prompt.trim() : "";
  if (!prompt || prompt.length > 2000) return "prompt must contain 1 to 2000 characters";
  const model = b.model === undefined ? DEFAULT_VIDEO_MODEL : b.model;
  if (typeof model !== "string" || !(VIDEO_MODELS as readonly string[]).includes(model)) return `model must be one of: ${VIDEO_MODELS.join(", ")}`;
  if (b.image !== undefined || b.image_base64 !== undefined || b.image_url !== undefined) return "image inputs are not accepted; send a text prompt";
  if (b.adult === true && KEYFRAME_VIDEO_MODELS.includes(model)) {
    return `${model} video animates a drawn first frame, and adult mode never takes an image input; turn adult mode off or use the default video model`;
  }
  const adultModel = opts.adultModel && (VIDEO_MODELS as readonly string[]).includes(opts.adultModel) && !KEYFRAME_VIDEO_MODELS.includes(opts.adultModel) ? opts.adultModel : ADULT_VIDEO_MODEL;
  const out: VideoRequest = { prompt, model: b.adult === true && model === DEFAULT_VIDEO_MODEL ? adultModel : model };
  if (typeof b.negative_prompt === "string" && b.negative_prompt.trim()) out.negative_prompt = b.negative_prompt.trim().slice(0, 2000);
  if (b.seed !== undefined && b.seed !== null) {
    const seed = Number(b.seed);
    if (!Number.isInteger(seed) || seed < 0 || seed > 4294967295) return "seed must be an integer from 0 to 4294967295";
    out.seed = seed;
  }
  if (b.seconds !== undefined && b.seconds !== null) {
    const n = Number(b.seconds);
    if (!Number.isFinite(n)) return `seconds must be a number from ${MIN_VIDEO_SECONDS} to ${MAX_VIDEO_SECONDS}`;
    out.seconds = Math.max(MIN_VIDEO_SECONDS, Math.min(MAX_VIDEO_SECONDS, Math.round(n)));
  }
  if (b.vertical === true) out.vertical = true;
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

export type VideoCommand = { prompt: string; model: string; seconds?: number; vertical?: true };

/**
 * `/video a paper boat on a pond` (also /clip, /vid): a 30 s clip via POST
 * /api/v1/videos (Wan 2.2 5B, planned 5 s shots). `--seconds N` (5-60, also
 * --duration/--length, or just write "a 15 second video") sets the length;
 * `--vertical` renders 9:16 for phones. `--model wan2.1-t2v-1.3b` is a quick
 * ~2 s CPU draft; `--model chatrxl-uncensored` draws the first frame with
 * ChatrXL and animates it.
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
  let seconds: number | undefined;
  const len = /(?:^|\s)--(?:seconds|duration|length|secs?)[=\s]+(\S+)/i.exec(rest);
  if (len) {
    const n = Number(len[1]!.replace(/s$/i, ""));
    if (!Number.isFinite(n) || n <= 0) return `--seconds must be a number from ${MIN_VIDEO_SECONDS} to ${MAX_VIDEO_SECONDS}`;
    seconds = Math.max(MIN_VIDEO_SECONDS, Math.min(MAX_VIDEO_SECONDS, Math.round(n)));
    rest = rest.replace(len[0], " ").trim();
  }
  let vertical = false;
  const vert = /(?:^|\s)--(?:vertical|portrait|9:16)(?=\s|$)/i.exec(rest);
  if (vert) {
    vertical = true;
    rest = rest.replace(vert[0], " ").trim();
  }
  if (!rest) return "usage: /video <what happens in the clip>";
  if (rest.length > 2000) return "prompt must contain 1 to 2000 characters";
  const out: VideoCommand = { prompt: rest, model };
  if (seconds !== undefined) out.seconds = seconds;
  if (vertical) out.vertical = true;
  return out;
}

/**
 * The POST /api/v1/videos body for a command. `adult` is the chat's 18+ mode
 * (attested account AND the switch on), exactly as /image sends it: the key
 * is present only when true, never `adult: false`.
 */
export function videoRequestBody(cmd: VideoCommand, adult: boolean): { prompt: string; model: string; seconds?: number; vertical?: true; adult?: true } {
  const body: { prompt: string; model: string; seconds?: number; vertical?: true; adult?: true } = { prompt: cmd.prompt, model: cmd.model };
  if (cmd.seconds !== undefined) body.seconds = cmd.seconds;
  if (cmd.vertical) body.vertical = true;
  if (adult) body.adult = true;
  return body;
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
