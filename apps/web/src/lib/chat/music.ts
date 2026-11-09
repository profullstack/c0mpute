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
export const MAX_SECONDS = 120;

/** Validate a POST body; returns the body to forward or an error string. */
export function parseMusicRequest(
  body: unknown,
): { prompt: string; lyrics: string; duration: number } | string {
  const b = (body ?? {}) as Record<string, unknown>;
  const prompt = typeof b.prompt === "string" ? b.prompt.trim() : "";
  // Long briefs and whole songs are fine; Infernet clips what the model reads.
  if (!prompt || prompt.length > MAX_TEXT_CHARS) return `prompt must contain 1 to ${MAX_TEXT_CHARS} characters`;
  const lyrics = typeof b.lyrics === "string" ? b.lyrics.trim() : "";
  if (lyrics.length > MAX_TEXT_CHARS) return `lyrics must be at most ${MAX_TEXT_CHARS} characters`;
  const duration = b.duration === undefined || b.duration === null ? 30 : Number(b.duration);
  if (!Number.isInteger(duration) || duration < MIN_SECONDS || duration > MAX_SECONDS) {
    return `duration must be a whole number of seconds from ${MIN_SECONDS} to ${MAX_SECONDS}`;
  }
  return { prompt, lyrics, duration };
}

export const AUDIO_TYPES = ["audio/mpeg", "audio/wav", "audio/flac", "audio/ogg"];

export function audioExtension(type: string): string {
  return type === "audio/wav" ? "wav" : type === "audio/flac" ? "flac" : type === "audio/ogg" ? "ogg" : "mp3";
}
