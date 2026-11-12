// Model profiles: what each live model is, what it costs, and how good it is
// likely to be at each kind of task before the router has any data on it.
//
// The network only tells us model ids and prices, so the profile is parsed
// from the id (family, size, coder/specialist/uncensored) and the priors
// come from size and family. Learned scores (stats.ts) take over as real
// outcomes arrive.

import type { Category, ModelProfile, ModelTag } from "./types";

export type PriceRow = {
  model: string;
  input_per_m_usd: number;
  output_per_m_usd: number;
  live?: boolean;
  market?: { providers?: number; busy?: number };
};

export type PriceTable = { default_model_price?: { input_per_m_usd: number; output_per_m_usd: number }; models?: PriceRow[] };

/** House fine-tunes and the one category each is for. */
const SPECIALISTS: [RegExp, Category][] = [
  [/not-a-lawyer|lawyer|legal/i, "legal"],
  [/tax-guy|tax/i, "tax"],
  [/saas-business|business-strategist|strategist/i, "business"],
];

const CONTEXT: [RegExp, number][] = [
  [/deepseek-coder-v2/i, 128_000],
  [/qwen3/i, 32_768],
  [/qwen2\.5/i, 32_768],
  [/llama3\.[1-3]/i, 128_000],
  [/gemma3/i, 128_000],
  [/nemotron/i, 128_000],
  [/mistral|mixtral/i, 32_768],
];

export function parseModel(id: string): Omit<ModelProfile, "priceIn" | "priceOut" | "live"> {
  const lower = id.toLowerCase();
  const name = lower.replace(/^hf\.co\/[^/]+\//, "").replace(/^[^/]+\//, "");
  const size = name.match(/[:\-_](\d+(?:\.\d+)?)b\b/) ?? name.match(/(\d+(?:\.\d+)?)b\b/);
  const paramsB = size ? Number(size[1]) : null;
  const family = name.split(/[:]/)[0].replace(/-?\d+(\.\d+)?b$/, "");
  const tags: ModelTag[] = [];
  let specialty: Category | undefined;
  if (/coder|code/.test(name)) tags.push("coder");
  if (/abliterated|uncensored|nsfw/.test(lower)) tags.push("uncensored");
  if (/qwen3|deepseek-r1|reasoning|thinking|qwq/.test(name)) tags.push("reasoning");
  if (paramsB !== null && paramsB < 1.6) tags.push("tiny");
  if (/^hf\.co\/profullstack\//.test(lower) || /-agent\b/.test(name)) {
    for (const [re, cat] of SPECIALISTS) {
      if (re.test(name)) {
        specialty = cat;
        tags.push("specialist");
        break;
      }
    }
  }
  const context = CONTEXT.find(([re]) => re.test(name))?.[1] ?? 8_192;
  return { id, family, paramsB, context, tags, specialty };
}

export function buildProfiles(liveIds: string[], prices?: PriceTable): ModelProfile[] {
  const def = prices?.default_model_price ?? { input_per_m_usd: 0.03, output_per_m_usd: 0.12 };
  const byId = new Map((prices?.models ?? []).map((r) => [r.model, r]));
  return liveIds.map((id) => {
    const row = byId.get(id);
    return {
      ...parseModel(id),
      priceIn: row?.input_per_m_usd ?? def.input_per_m_usd,
      priceOut: row?.output_per_m_usd ?? def.output_per_m_usd,
      live: row?.live ?? true,
      providers: row?.market?.providers,
      busy: row?.market?.busy,
    };
  });
}

const clamp = (x: number, lo = 0.02, hi = 0.97) => Math.max(lo, Math.min(hi, x));

/**
 * Prior probability (0..1) that this model gives an acceptable answer to an
 * average task of `category`. Size dominates (log scale: 0.5B ~0.25, 7B
 * ~0.55, 70B ~0.8), then family and specialization adjust it.
 */
export function prior(m: ModelProfile, category: Category): number {
  const b = m.paramsB ?? 7;
  const span = Math.log2(70 / 0.5);
  let q = 0.25 + 0.55 * Math.max(0, Math.min(1.15, Math.log2(Math.max(0.3, b) / 0.5) / span));
  if (/qwen3\.5/.test(m.family)) q += 0.07;
  else if (/qwen3/.test(m.family)) q += 0.05;
  if (/nemotron/.test(m.family)) q += 0.03;
  const coder = m.tags.includes("coder");
  if (coder) {
    if (category === "code") q += 0.12;
    else if (category === "math" || category === "extract") q += 0.03;
    else if (category === "chat" || category === "writing" || category === "translate") q -= 0.1;
    else q -= 0.12; // summarize, reasoning, legal, tax, business
  }
  if (/deepseek-coder-v2/.test(m.family) && category === "code") q += 0.05;
  if (m.tags.includes("reasoning") && (category === "math" || category === "reasoning")) q += 0.05;
  if (m.specialty) {
    // A fine-tune on a small base: strong in its lane, weak elsewhere.
    q = m.specialty === category ? q + 0.25 : q - 0.2;
  }
  if (m.tags.includes("uncensored")) q -= 0.03;
  return clamp(q);
}
