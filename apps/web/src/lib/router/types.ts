// Model router types. Framework-free: nothing in lib/router imports Next,
// the database or the network, so it can move to its own package unchanged.

export type Role = "system" | "user" | "assistant";
export type Msg = { role: Role; content: string };

/** How hard to try. `fast` = one model, no checks; `max` = parallel drafts + synthesis. */
export type Effort = "fast" | "standard" | "high" | "max";
export const EFFORTS: Effort[] = ["fast", "standard", "high", "max"];

export type Category =
  | "chat"
  | "code"
  | "math"
  | "reasoning"
  | "extract"
  | "writing"
  | "summarize"
  | "translate"
  | "legal"
  | "tax"
  | "business";

export type Features = {
  category: Category;
  /** Runner-up category and its share of the signal (0..1), for transparency. */
  runnerUp?: { category: Category; share: number };
  /** 0 (trivial) .. 1 (hard). */
  difficulty: number;
  /** Rough token count of everything sent. */
  promptTokens: number;
  /** Output must be JSON (asked for, or response_format). */
  wantsJson: boolean;
  /** The user wrote mostly non-Latin script (answer should match). */
  nonLatin: boolean;
  /** Why the classifier decided what it did. */
  reasons: string[];
};

export type ModelTag = "coder" | "specialist" | "uncensored" | "reasoning" | "tiny";

export type ModelProfile = {
  id: string;
  family: string;
  /** Parameters in billions, when the id says. */
  paramsB: number | null;
  /** Context window in tokens (conservative). */
  context: number;
  tags: ModelTag[];
  /** The one category a fine-tuned specialist is for. */
  specialty?: Category;
  /** USD per 1M tokens. */
  priceIn: number;
  priceOut: number;
  live: boolean;
  /** Serving nodes and how many are busy, when known. */
  providers?: number;
  busy?: number;
};

export type PlanKind = "single" | "cascade" | "critique" | "fuse";

export type Estimate = { quality: number; costUsd: number; latencyMs: number };

export type Plan = {
  kind: PlanKind;
  /** The model that drafts (single/cascade/critique) or synthesizes (fuse). */
  primary: string;
  /** Cascade: the stronger model a rejected draft escalates to. */
  escalate?: string;
  /** Cascade/fuse: the LLM judge (absent = deterministic checks only). */
  judge?: string;
  critics?: string[];
  drafters?: string[];
  /** Fallbacks, best first, when a call fails outright. */
  fallbacks: string[];
  expected: Estimate;
  reasons: string[];
};

export type PlanOption = { kind: PlanKind; models: string[] } & Estimate;

export type RouteOptions = {
  effort: Effort;
  /** Hard ceiling on what the request may spend, USD. */
  maxCostUsd?: number;
  /** Hard ceiling on wall time, ms. */
  maxLatencyMs?: number;
  /** Allow uncensored ("abliterated") models (adult mode). */
  allowUncensored?: boolean;
  maxTokens?: number;
  temperature?: number;
  /**
   * Hedge the first draft: if it has not answered after this many ms, send
   * the same request to the next model too and keep whichever finishes
   * first. Set by the router from the plan's expected latency.
   */
  hedgeAfterMs?: number;
};

export type Verification = { pass: boolean; score: number; issues: string[] };

export type StepRole = "draft" | "verify" | "judge" | "escalate" | "critic" | "revise" | "fallback" | "synthesize";

export type StepRecord = {
  role: StepRole;
  model: string;
  ok: boolean;
  ms: number;
  inTokens: number;
  outTokens: number;
  costUsd: number;
  note?: string;
};

export type Receipt = {
  router: "c0mpute-router/1";
  features: Features;
  plan: Plan;
  alternatives: PlanOption[];
  steps: StepRecord[];
  final: { model: string; via: PlanKind; verification: Verification };
  escalated: boolean;
  totalCostUsd: number;
  totalMs: number;
  budget: { maxCostUsd?: number; maxLatencyMs?: number; hit?: "cost" | "latency" };
};

/** One model call. The router never talks to the network itself. */
export type CallFn = (
  model: string,
  messages: Msg[],
  opts: { maxTokens?: number; temperature?: number; signal?: AbortSignal },
) => Promise<{ text: string; model?: string; finishReason?: string; inTokens?: number; outTokens?: number }>;
