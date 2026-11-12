// What the router has learned: per (model, category) success counts, and per
// model health (latency, recent errors).
//
// Quality is a Beta posterior: the prior counts as PRIOR_WEIGHT pseudo-
// observations, each real outcome adds one. A result that passed the checks
// and was accepted is a success; a draft that failed verification, was
// rejected by a judge, or errored is a failure. So routing improves with use
// and a model that starts misbehaving loses traffic within a few requests.

import type { Category } from "./types";

export const PRIOR_WEIGHT = 6;
/** Errors in a row before a model sits out. */
export const ERROR_STREAK_COOLDOWN = 3;
export const COOLDOWN_MS = 2 * 60_000;
const EWMA = 0.2;
/** One observation counts as at most this slow (ms per output token). */
export const MAX_SAMPLE_MS_PER_TOKEN = 400;

export type QualityRow = { ok: number; fail: number };
export type HealthRow = {
  /** EWMA ms per output token (generation speed). */
  msPerToken?: number;
  /** EWMA ms to first byte / fixed overhead. */
  overheadMs?: number;
  /** EWMA error rate 0..1. */
  errorRate: number;
  errorStreak: number;
  coolUntil?: number;
  calls: number;
};

export type Snapshot = {
  v: 1;
  quality: Record<string, QualityRow>; // key: model \u0001 category
  health: Record<string, HealthRow>;
};

const key = (model: string, category: Category) => `${model}\u0001${category}`;

export class Stats {
  quality = new Map<string, QualityRow>();
  health = new Map<string, HealthRow>();
  /** Bumped on every change; a store saves when it moved. */
  version = 0;

  static from(s?: Snapshot | null): Stats {
    const st = new Stats();
    if (s?.v === 1) {
      for (const [k, v] of Object.entries(s.quality ?? {})) st.quality.set(k, { ok: +v.ok || 0, fail: +v.fail || 0 });
      for (const [k, v] of Object.entries(s.health ?? {})) st.health.set(k, { ...v, coolUntil: undefined, errorStreak: 0 });
    }
    return st;
  }

  snapshot(): Snapshot {
    return { v: 1, quality: Object.fromEntries(this.quality), health: Object.fromEntries(this.health) };
  }

  /** Posterior mean: the prior as PRIOR_WEIGHT pseudo-counts plus real outcomes. */
  qualityOf(model: string, category: Category, priorQ: number): number {
    const r = this.quality.get(key(model, category));
    if (!r) return priorQ;
    return (priorQ * PRIOR_WEIGHT + r.ok) / (PRIOR_WEIGHT + r.ok + r.fail);
  }

  observations(model: string, category: Category): number {
    const r = this.quality.get(key(model, category));
    return r ? r.ok + r.fail : 0;
  }

  recordOutcome(model: string, category: Category, ok: boolean) {
    const k = key(model, category);
    const r = this.quality.get(k) ?? { ok: 0, fail: 0 };
    if (ok) r.ok += 1;
    else r.fail += 1;
    // Cap total so old evidence decays and the model can recover.
    const total = r.ok + r.fail;
    if (total > 200) {
      r.ok *= 200 / total;
      r.fail *= 200 / total;
    }
    this.quality.set(k, r);
    this.version++;
  }

  recordCall(model: string, r: { ok: boolean; ms: number; outTokens: number }, now = Date.now()) {
    const h = this.health.get(model) ?? { errorRate: 0, errorStreak: 0, calls: 0 };
    h.calls += 1;
    h.errorRate = h.errorRate * (1 - EWMA) + (r.ok ? 0 : 1) * EWMA;
    if (r.ok) {
      h.errorStreak = 0;
      h.coolUntil = undefined;
      if (r.outTokens > 8) {
        // Split wall time into overhead + per-token cost with a fixed 400 ms overhead guess.
        // Capped: a cold model load on a node (60-100 s once) must not
        // read as the model's speed forever; EWMA does the rest.
        const per = Math.min(MAX_SAMPLE_MS_PER_TOKEN, Math.max(1, (r.ms - 400) / r.outTokens));
        h.msPerToken = h.msPerToken === undefined ? per : h.msPerToken * (1 - EWMA) + per * EWMA;
        h.overheadMs = h.overheadMs === undefined ? 400 : h.overheadMs * (1 - EWMA) + 400 * EWMA;
      }
    } else {
      h.errorStreak += 1;
      if (h.errorStreak >= ERROR_STREAK_COOLDOWN) h.coolUntil = now + COOLDOWN_MS;
    }
    this.health.set(model, h);
    this.version++;
  }

  /** Sitting out after repeated errors. */
  cooling(model: string, now = Date.now()): boolean {
    const h = this.health.get(model);
    return !!h?.coolUntil && h.coolUntil > now;
  }

  errorRate(model: string): number {
    return this.health.get(model)?.errorRate ?? 0;
  }

  msPerToken(model: string): number | undefined {
    return this.health.get(model)?.msPerToken;
  }
}

/** Where learned stats live between processes. */
export interface StatsStore {
  load(): Promise<Snapshot | null>;
  save(s: Snapshot): Promise<void>;
}

export class MemoryStore implements StatsStore {
  data: Snapshot | null = null;
  async load() {
    return this.data;
  }
  async save(s: Snapshot) {
    this.data = JSON.parse(JSON.stringify(s));
  }
}
