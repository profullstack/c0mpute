// The planner: given a classified task, the live models and what we have
// learned, pick an execution plan by expected utility.
//
// Every candidate plan gets an estimate of quality (probability the final
// answer is acceptable), cost and latency, from per-model quality (prior +
// learned), prices and measured speed. The effort level decides which plan
// shapes are allowed and how much a point of quality is worth against a cent
// or a second. A cascade is only chosen when its expected cost (draft +
// check + escalation probability x the strong model) beats going straight to
// the strong model at the quality wanted, which is the check Copilot Fusion
// does not show it makes.

import { prior } from "./registry";
import { Stats } from "./stats";
import type { Category, Effort, Estimate, Features, ModelProfile, Plan, PlanOption, RouteOptions } from "./types";

/** Typical answer length by task, tokens (scaled up by difficulty). */
const OUT_TOKENS: Record<Category, number> = {
  chat: 180,
  code: 700,
  math: 450,
  reasoning: 550,
  extract: 300,
  writing: 650,
  summarize: 300,
  translate: 350,
  legal: 600,
  tax: 600,
  business: 600,
};

/** Weight of cost (per USD) and latency (per second) against quality, by effort. */
const WEIGHTS: Record<Effort, { cost: number; latency: number }> = {
  fast: { cost: 20, latency: 0.02 },
  standard: { cost: 10, latency: 0.008 },
  high: { cost: 4, latency: 0.003 },
  max: { cost: 1, latency: 0.001 },
};

/**
 * No plan may fall below this share of the best single model's quality,
 * whatever it saves: `fast` means quick, not wrong.
 */
const QUALITY_FLOOR: Record<Effort, number> = { fast: 0.75, standard: 0.85, high: 0.9, max: 0.95 };

/** Multi-model patterns only pay off on real work, not small talk. */
const WORTH_ORCHESTRATING = (f: Features) =>
  f.difficulty >= 0.25 || ["code", "math", "reasoning", "legal", "tax", "business", "extract"].includes(f.category);

/** How often checks catch a bad draft: deterministic only vs with an LLM judge. */
const RECALL_VERIFY = 0.45;
const RECALL_JUDGE = 0.8;
/** How often a good draft is wrongly escalated. */
const FALSE_ESCALATE_VERIFY = 0.03;
const FALSE_ESCALATE_JUDGE = 0.12;
const JUDGE_OUT_TOKENS = 60;
const CRITIC_OUT_TOKENS = 200;

export type ModelView = {
  m: ModelProfile;
  /** P(acceptable) for this task. */
  p: number;
  costUsd: number;
  latencyMs: number;
};

export function outTokensFor(f: Features, maxTokens?: number): number {
  const t = Math.round(OUT_TOKENS[f.category] * (0.6 + f.difficulty * 1.2));
  return Math.max(16, Math.min(maxTokens ?? 4096, t));
}

function speedMsPerToken(m: ModelProfile, stats: Stats): number {
  const learned = stats.msPerToken(m.id);
  if (learned !== undefined) return learned;
  // CPU-heavy network: ~6 ms/token for 0.5B up to ~60 ms/token for 16B.
  return 6 + 3.4 * (m.paramsB ?? 7);
}

export function viewModels(models: ModelProfile[], f: Features, stats: Stats, opts: RouteOptions, now = Date.now()): ModelView[] {
  const out = outTokensFor(f, opts.maxTokens);
  return models
    .filter((m) => m.live)
    .filter((m) => opts.allowUncensored || !m.tags.includes("uncensored"))
    .filter((m) => !stats.cooling(m.id, now))
    .filter((m) => m.context >= f.promptTokens + out)
    .map((m) => {
      let p = stats.qualityOf(m.id, f.category, prior(m, f.category));
      // Difficulty bends acceptance: easy tasks are easy for everyone
      // (q^0.4), hard ones punish weak models hardest (q^2).
      p = Math.pow(p, 0.4 + 1.6 * f.difficulty);
      // Recent errors and a saturated pool count against it.
      p *= 1 - Math.min(0.5, stats.errorRate(m.id));
      if (m.providers !== undefined && m.busy !== undefined && m.providers > 0 && m.busy >= m.providers) p *= 0.92;
      p = Math.max(0.01, Math.min(0.99, p));
      const costUsd = (f.promptTokens * m.priceIn + out * m.priceOut) / 1e6;
      const latencyMs = 400 + out * speedMsPerToken(m, stats) + f.promptTokens * 0.15;
      return { m, p, costUsd, latencyMs };
    })
    .sort((a, b) => b.p - a.p || a.costUsd - b.costUsd || a.m.id.localeCompare(b.m.id));
}

function utility(e: Estimate, effort: Effort): number {
  const w = WEIGHTS[effort];
  return e.quality - w.cost * e.costUsd - w.latency * (e.latencyMs / 1000);
}

function fits(e: Estimate, opts: RouteOptions): boolean {
  if (opts.maxCostUsd !== undefined && e.costUsd > opts.maxCostUsd) return false;
  if (opts.maxLatencyMs !== undefined && e.latencyMs > opts.maxLatencyMs) return false;
  return true;
}

type Candidate = { plan: Omit<Plan, "fallbacks" | "reasons">; est: Estimate; why: string };

/** A judge should be decent and not the drafter (models over-rate themselves). */
function pickJudge(views: ModelView[], exclude: string[]): ModelView | undefined {
  return views
    .filter((v) => !exclude.includes(v.m.id) && !v.m.specialty && !v.m.tags.includes("tiny"))
    .sort((a, b) => b.p - a.p || a.costUsd - b.costUsd)[0];
}

export function candidates(views: ModelView[], f: Features, opts: RouteOptions): Candidate[] {
  const out: Candidate[] = [];
  const effort = opts.effort;
  const judgeCost = (j: ModelView) => ((f.promptTokens + outTokensFor(f, opts.maxTokens)) * j.m.priceIn + JUDGE_OUT_TOKENS * j.m.priceOut) / 1e6;
  const judgeMs = (j: ModelView) => 400 + JUDGE_OUT_TOKENS * (j.latencyMs / Math.max(1, outTokensFor(f, opts.maxTokens)));

  for (const v of views) {
    out.push({ plan: { kind: "single", primary: v.m.id, expected: { quality: v.p, costUsd: v.costUsd, latencyMs: v.latencyMs } }, est: { quality: v.p, costUsd: v.costUsd, latencyMs: v.latencyMs }, why: `single ${v.m.id}` });
  }
  if (effort === "fast" || views.length < 2) return out;

  // Cascade: cheap draft, checked, escalated to a stronger model on failure.
  const strong = views.slice(0, 3);
  for (const b of strong) {
    for (const a of views) {
      if (a.m.id === b.m.id || a.costUsd >= b.costUsd * 0.8) continue;
      const withJudge = effort === "high" || effort === "max";
      const j = withJudge ? pickJudge(views, [a.m.id]) : undefined;
      const recall = j ? RECALL_JUDGE : RECALL_VERIFY;
      const fp = j ? FALSE_ESCALATE_JUDGE : FALSE_ESCALATE_VERIFY;
      const pEsc = (1 - a.p) * recall + a.p * fp;
      const quality = a.p * (1 - fp) + pEsc * b.p;
      const costUsd = a.costUsd + (j ? judgeCost(j) : 0) + pEsc * b.costUsd;
      const latencyMs = a.latencyMs + (j ? judgeMs(j) : 0) + pEsc * b.latencyMs;
      const est = { quality, costUsd, latencyMs };
      out.push({
        plan: { kind: "cascade", primary: a.m.id, escalate: b.m.id, judge: j?.m.id, expected: est },
        est,
        why: `cascade ${a.m.id} -> ${b.m.id}${j ? ` judged by ${j.m.id}` : " (checks only)"}: escalates ${Math.round(pEsc * 100)}% of the time`,
      });
    }
  }

  if ((effort === "high" || effort === "max") && WORTH_ORCHESTRATING(f)) {
    // Critique: draft, 2 different critics in parallel, revise.
    for (const v of views.slice(0, 3)) {
      const critics = views.filter((c) => c.m.id !== v.m.id && !c.m.specialty).slice(0, 2);
      if (critics.length === 0) continue;
      const criticP = critics.reduce((n, c) => n + c.p, 0) / critics.length;
      const uplift = 0.3 * criticP;
      const quality = 1 - (1 - v.p) * (1 - uplift);
      const critCost = critics.reduce((n, c) => n + ((f.promptTokens + outTokensFor(f, opts.maxTokens)) * c.m.priceIn + CRITIC_OUT_TOKENS * c.m.priceOut) / 1e6, 0);
      const costUsd = v.costUsd * 2.2 + critCost;
      const latencyMs = v.latencyMs * 2 + Math.max(...critics.map((c) => 400 + CRITIC_OUT_TOKENS * (c.latencyMs / Math.max(1, outTokensFor(f, opts.maxTokens)))));
      const est = { quality, costUsd, latencyMs };
      out.push({ plan: { kind: "critique", primary: v.m.id, critics: critics.map((c) => c.m.id), expected: est }, est, why: `critique: ${v.m.id} drafts, ${critics.map((c) => c.m.id).join(" + ")} review` });
    }
  }

  if (effort === "max" && views.length >= 3 && WORTH_ORCHESTRATING(f)) {
    // Fuse: 3 diverse strong drafters in parallel, the best model synthesizes.
    const drafters: ModelView[] = [];
    const families = new Set<string>();
    for (const v of views) {
      if (drafters.length === 3) break;
      if (families.has(v.m.family) && views.length > 4) continue;
      families.add(v.m.family);
      drafters.push(v);
    }
    const synth = views[0];
    const missAll = drafters.reduce((n, d) => n * (1 - d.p), 1);
    const quality = Math.min(0.99, (1 - missAll) * 0.9 + synth.p * 0.1);
    const synthCost = ((f.promptTokens + drafters.length * outTokensFor(f, opts.maxTokens)) * synth.m.priceIn + outTokensFor(f, opts.maxTokens) * synth.m.priceOut) / 1e6;
    const costUsd = drafters.reduce((n, d) => n + d.costUsd, 0) + synthCost;
    const latencyMs = Math.max(...drafters.map((d) => d.latencyMs)) + synth.latencyMs;
    const est = { quality, costUsd, latencyMs };
    out.push({ plan: { kind: "fuse", primary: synth.m.id, drafters: drafters.map((d) => d.m.id), judge: synth.m.id, expected: est }, est, why: `fuse: ${drafters.map((d) => d.m.id).join(", ")} draft in parallel, ${synth.m.id} synthesizes` });
  }
  return out;
}

export function plan(models: ModelProfile[], f: Features, stats: Stats, opts: RouteOptions, now = Date.now()): { plan: Plan; alternatives: PlanOption[]; views: ModelView[] } {
  const views = viewModels(models, f, stats, opts, now);
  if (views.length === 0) throw new Error("no live model can take this request");
  const bestSingle = views[0].p;
  const floor = bestSingle * QUALITY_FLOOR[opts.effort];
  const all = candidates(views, f, opts);
  const fitting = all.filter((c) => fits(c.est, opts) && c.est.quality >= floor);
  const pool = fitting.length ? fitting : all.filter((c) => c.plan.kind === "single");
  const scored = pool
    .map((c) => ({ c, u: utility(c.est, opts.effort) }))
    .sort((a, b) => b.u - a.u || a.c.est.costUsd - b.c.est.costUsd || a.c.plan.primary.localeCompare(b.c.plan.primary));
  // Over budget everywhere: the cheapest single model.
  const best = fitting.length ? scored[0].c : [...pool].sort((a, b) => a.est.costUsd - b.est.costUsd)[0];

  const used = new Set([best.plan.primary, best.plan.escalate, ...(best.plan.drafters ?? [])].filter(Boolean) as string[]);
  const fallbacks = views.filter((v) => !used.has(v.m.id)).slice(0, 3).map((v) => v.m.id);
  const reasons = [
    ...f.reasons,
    `effort ${opts.effort}: ${best.why}`,
    `expected quality ${pct(best.est.quality)}, cost $${best.est.costUsd.toFixed(6)}, ~${Math.round(best.est.latencyMs / 100) / 10}s`,
  ];
  if (!fitting.length) reasons.push("no plan fits the budget: cheapest model chosen");
  const specialist = views.find((v) => v.m.specialty === f.category);
  if (specialist && best.plan.primary === specialist.m.id) reasons.push(`house specialist for ${f.category}`);

  const seen = new Set<string>();
  const alternatives: PlanOption[] = [];
  for (const { c } of scored) {
    const k = `${c.plan.kind}:${c.plan.primary}:${c.plan.escalate ?? ""}`;
    if (seen.has(k) || c === best) continue;
    seen.add(k);
    alternatives.push({ kind: c.plan.kind, models: [c.plan.primary, c.plan.escalate, ...(c.plan.critics ?? []), ...(c.plan.drafters ?? [])].filter(Boolean) as string[], ...c.est });
    if (alternatives.length >= 4) break;
  }
  return { plan: { ...best.plan, fallbacks, reasons }, alternatives, views };
}

const pct = (x: number) => `${Math.round(x * 100)}%`;
