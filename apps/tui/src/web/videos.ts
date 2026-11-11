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
