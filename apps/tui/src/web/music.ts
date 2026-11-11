// Music generation in c0mpute chat. The work runs on c0mpute workers
// (music.txt2music, ACE-Step 1.5) behind Infernet's /api/chat/music, which
// owns the free tier and per-user billing; c0mpute.com only forwards, naming
// the signed-in person through the trusted-client subject header.

export type MusicCommand = { prompt: string; lyrics: string };

/**
 * `/music lo-fi hip hop` (also /song) -> { prompt, lyrics: "" }. Lyrics follow
 * a line that is exactly `lyrics:`, or a blank line after the style; no
 * lyrics means an instrumental. Anything else -> null.
 */
export function parseMusicCommand(text: string): MusicCommand | null {
  const m = /^\/(?:music|song)\s+([\s\S]+)$/i.exec(String(text ?? "").trim());
  if (!m) return null;
  const body = m[1].trim();
  const split = /\n\s*lyrics:\s*\n|\n\s*\n/i.exec(body);
  const prompt = (split ? body.slice(0, split.index) : body).trim();
  const lyrics = split ? body.slice(split.index + split[0].length).trim() : "";
  return prompt ? { prompt, lyrics } : null;
}

export const MAX_TEXT_CHARS = 100_000;
export const MIN_SECONDS = 10;
export const MAX_SECONDS = 180;

/** Validate a POST body; returns the body to forward or an error string. */
export function parseMusicRequest(
  body: unknown,
): { prompt: string; lyrics: string; duration?: number } | string {
  const b = (body ?? {}) as Record<string, unknown>;
  const prompt = typeof b.prompt === "string" ? b.prompt.trim() : "";
  // Long briefs and whole songs are fine; Infernet clips what the model reads.
  if (!prompt || prompt.length > MAX_TEXT_CHARS) return `prompt must contain 1 to ${MAX_TEXT_CHARS} characters`;
  const lyrics = typeof b.lyrics === "string" ? b.lyrics.trim() : "";
  if (lyrics.length > MAX_TEXT_CHARS) return `lyrics must be at most ${MAX_TEXT_CHARS} characters`;
  // No duration means "whatever the brief asks for": Infernet reads a length
  // from the prompt ("150-second song") or lets its writer pick. Defaulting
  // to 30 here overrode that and every c0mpute.com song came out 30 s.
  if (b.duration === undefined || b.duration === null) return { prompt, lyrics };
  const duration = Number(b.duration);
  if (!Number.isInteger(duration) || duration < MIN_SECONDS || duration > MAX_SECONDS) {
    return `duration must be a whole number of seconds from ${MIN_SECONDS} to ${MAX_SECONDS}`;
  }
  return { prompt, lyrics, duration };
}

/** What the c0mpute node doing a song reports, relayed by Infernet. */
export type NodeProgress = {
  stage: string;
  worker?: string;
  eta_s?: number;
  elapsed_s?: number;
  stage_elapsed_s?: number;
  attempt?: number;
  detail?: string;
  queue_position?: number;
};

const STAGES = ["starting", "finding_worker", "queued", "rendering", "fetching_result", "retrying", "done", "failed"];

/** Keep only known, well-typed fields of an upstream progress object. */
export function sanitizeNodeProgress(v: unknown): NodeProgress | undefined {
  if (!v || typeof v !== "object") return undefined;
  const o = v as Record<string, unknown>;
  if (typeof o.stage !== "string" || !STAGES.includes(o.stage)) return undefined;
  const num = (k: string) => (typeof o[k] === "number" && Number.isFinite(o[k]) && (o[k] as number) >= 0 ? (o[k] as number) : undefined);
  const str = (k: string, max: number) => (typeof o[k] === "string" ? (o[k] as string).slice(0, max) : undefined);
  return {
    stage: o.stage,
    worker: str("worker", 80),
    eta_s: num("eta_s"),
    elapsed_s: num("elapsed_s"),
    stage_elapsed_s: num("stage_elapsed_s"),
    attempt: num("attempt"),
    detail: str("detail", 200),
    queue_position: num("queue_position"),
  };
}

const mmss = (s: number) => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, "0")}`;

/** One line for the chat: what the network is doing with this song now. */
export function describeNodeProgress(p: NodeProgress): string {
  const node = p.worker ? `node …${p.worker.slice(-8)}` : "a node";
  const retry = (p.attempt ?? 1) > 1 ? ` (attempt ${p.attempt})` : "";
  switch (p.stage) {
    case "starting":
    case "finding_worker":
      return `finding a c0mpute music worker${retry}`;
    case "queued":
      // The gateway holds the song (up to 2 h) until a music worker is free.
      return `queued for a c0mpute music worker${p.queue_position ? ` (#${p.queue_position} in line)` : ""} · ${mmss(p.stage_elapsed_s ?? 0)}`;
    case "rendering": {
      const left = p.eta_s !== undefined ? Math.max(0, p.eta_s - (p.stage_elapsed_s ?? 0)) : undefined;
      const eta = left === undefined ? "" : left > 0 ? ` · ~${mmss(left)} left (node's estimate)` : " · past the node's estimate, still working";
      return `rendering on ${node}${retry} · ${mmss(p.stage_elapsed_s ?? 0)} in${eta}`;
    }
    case "fetching_result":
      return `${node} finished; fetching the song`;
    case "retrying":
      return `${node} failed${p.detail ? ` (${p.detail})` : ""}; trying another worker`;
    case "done":
      return "song ready";
    default:
      return "song failed";
  }
}

export const AUDIO_TYPES = ["audio/mpeg", "audio/wav", "audio/flac", "audio/ogg"];

export function audioExtension(type: string): string {
  return type === "audio/wav" ? "wav" : type === "audio/flac" ? "flac" : type === "audio/ogg" ? "ogg" : "mp3";
}
