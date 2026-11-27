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

/** Z-Image-Turbo (default) or ChatrXL Uncensored (SDXL/Pony, CPU workers that installed it). */
export const IMAGE_MODELS = ["z-image-turbo", "chatrxl-uncensored"] as const;
export type ImageModel = (typeof IMAGE_MODELS)[number];

/**
 * `a red bicycle --model chatrxl-uncensored` -> { prompt: "a red bicycle", model }.
 * The chat and the TUI send what follows /image as the prompt, so the flag
 * rides inside it. Returns an error string for an unknown model.
 */
export function splitImageModel(text: string): { prompt: string; model?: ImageModel } | string {
  const flag = /(?:^|\s)--model[=\s]+(\S+)/.exec(text);
  if (!flag) return { prompt: text.trim() };
  if (!(IMAGE_MODELS as readonly string[]).includes(flag[1]!)) return `model must be one of: ${IMAGE_MODELS.join(", ")}`;
  return { prompt: text.replace(flag[0], " ").trim(), model: flag[1] as ImageModel };
}

/**
 * Validate a POST body; returns the body to forward or an error string.
 * `adult: true` asks for adult (18+) mode; the route decides whether this
 * person may use it. Image inputs are not accepted here at all. `model` (or
 * a `--model` flag in the prompt) picks the image model; the default is left
 * off the forwarded body.
 */
export function parseImageRequest(body: unknown): { prompt: string; size: ImageSize; model?: ImageModel; adult?: true } | string {
  const b = (body ?? {}) as Record<string, unknown>;
  const split = splitImageModel(typeof b.prompt === "string" ? b.prompt : "");
  if (typeof split === "string") return split;
  const prompt = split.prompt;
  if (!prompt || prompt.length > 2000) return "prompt must contain 1 to 2000 characters";
  const size = (b.size ?? "landscape") as ImageSize;
  if (!IMAGE_SIZES.includes(size)) return `size must be one of: ${IMAGE_SIZES.join(", ")}`;
  if (b.model !== undefined && !(IMAGE_MODELS as readonly string[]).includes(b.model as string)) return `model must be one of: ${IMAGE_MODELS.join(", ")}`;
  const model = (b.model as ImageModel | undefined) ?? split.model;
  const out: { prompt: string; size: ImageSize; model?: ImageModel; adult?: true } = { prompt, size };
  if (model && model !== IMAGE_MODELS[0]) out.model = model;
  if (b.adult === true) out.adult = true;
  return out;
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
export const isJobId = (id: string) => UUID.test(id);

/** Messages that are image turns never go to the language model. */
export function textHistory<T extends { content: string; imageTurn?: boolean }>(msgs: T[]): T[] {
  return msgs.filter((m) => !m.imageTurn);
}
