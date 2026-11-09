// Image generation in c0mpute chat. The work runs on c0mpute CPU workers
// (diffusion.txt2img) behind Infernet's /api/chat/image, which owns the free
// tier and per-user billing; c0mpute.com only forwards, naming the signed-in
// person through the trusted-client subject header.

/** `/image a red bicycle` (also /img, /draw) -> "a red bicycle"; else null. */
export function parseImageCommand(text: string): string | null {
  const m = /^\/(?:image|img|draw)\s+([\s\S]+)$/i.exec(String(text ?? "").trim());
  return m ? m[1].trim() || null : null;
}

export const IMAGE_SIZES = ["landscape", "square", "portrait"] as const;
export type ImageSize = (typeof IMAGE_SIZES)[number];

/** Validate a POST body; returns the body to forward or an error string. */
export function parseImageRequest(body: unknown): { prompt: string; size: ImageSize } | string {
  const b = (body ?? {}) as Record<string, unknown>;
  const prompt = typeof b.prompt === "string" ? b.prompt.trim() : "";
  if (!prompt || prompt.length > 2000) return "prompt must contain 1 to 2000 characters";
  const size = (b.size ?? "landscape") as ImageSize;
  if (!IMAGE_SIZES.includes(size)) return `size must be one of: ${IMAGE_SIZES.join(", ")}`;
  return { prompt, size };
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
export const isJobId = (id: string) => UUID.test(id);

/** Messages that are image turns never go to the language model. */
export function textHistory<T extends { content: string; imageTurn?: boolean }>(msgs: T[]): T[] {
  return msgs.filter((m) => !m.imageTurn);
}
