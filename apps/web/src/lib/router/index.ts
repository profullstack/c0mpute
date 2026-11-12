// c0mpute model router: pick the model(s) and execution pattern for each
// request, run it, check it, and explain what happened.
//
//   const router = new Router({ call, models, store });
//   const { text, model, receipt } = await router.run(messages, { effort: "standard" });
//   const dry = await router.route(messages, { effort: "high" }); // plan only, no calls
//
// See DIP 0035 for the design and how it differs from Copilot Fusion.

import { classify } from "./classify";
import { execute, type RunResult } from "./engine";
import { plan as makePlan } from "./planner";
import { Stats, type StatsStore } from "./stats";
import { verify } from "./verify";
import { EFFORTS, type CallFn, type Effort, type Features, type ModelProfile, type Msg, type Plan, type PlanOption, type RouteOptions } from "./types";

export * from "./types";
export { classify } from "./classify";
export { buildProfiles, parseModel, prior, type PriceTable } from "./registry";
export { verify } from "./verify";
export { Stats, MemoryStore, type StatsStore, type Snapshot } from "./stats";

/** Hedge the draft at this multiple of its expected latency (per effort; none = never). */
const HEDGE_AT: Partial<Record<Effort, number>> = { fast: 1.4, standard: 2 };
/** Never hedge sooner than this: short answers finish before a hedge helps. */
const MIN_HEDGE_MS = 2500;

/** `auto`, `auto:fast`, `auto:standard`, `auto:high`, `auto:max` -> effort, else null. */
export function parseAutoModel(model: unknown): Effort | null {
  if (typeof model !== "string") return null;
  const m = model.trim().toLowerCase();
  if (m === "auto" || m === "router" || m === "c0/auto") return "standard";
  const e = m.replace(/^(auto|router|c0\/auto)[:/-]/, "");
  return e !== m && (EFFORTS as string[]).includes(e) ? (e as Effort) : null;
}

export type RouteDecision = { features: Features; plan: Plan; alternatives: PlanOption[] };

export class Router {
  readonly stats: Stats;
  private loaded: Promise<void> | null = null;
  private savedVersion = 0;
  private saveTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(
    private deps: {
      call: CallFn;
      /** Live models with prices (cache this; it is called per request). */
      models: () => Promise<ModelProfile[]>;
      store?: StatsStore;
      /** Debounce for persisting learned stats, ms. */
      saveEveryMs?: number;
      now?: () => number;
    },
  ) {
    this.stats = new Stats();
  }

  private async ensureLoaded() {
    this.loaded ??= (async () => {
      const snap = await this.deps.store?.load().catch(() => null);
      if (snap) {
        const s = Stats.from(snap);
        this.stats.quality = s.quality;
        this.stats.health = s.health;
      }
      this.savedVersion = this.stats.version;
    })();
    return this.loaded;
  }

  private scheduleSave() {
    if (!this.deps.store || this.stats.version === this.savedVersion || this.saveTimer) return;
    this.saveTimer = setTimeout(() => {
      this.saveTimer = null;
      this.savedVersion = this.stats.version;
      this.deps.store!.save(this.stats.snapshot()).catch(() => {});
    }, this.deps.saveEveryMs ?? 10_000);
    (this.saveTimer as { unref?: () => void }).unref?.();
  }

  /** Classify and plan, without calling any model. */
  async route(messages: Msg[], opts: RouteOptions, extra: { responseFormatJson?: boolean } = {}): Promise<RouteDecision> {
    await this.ensureLoaded();
    const features = classify(messages, extra);
    const models = await this.deps.models();
    const { plan, alternatives } = makePlan(models, features, this.stats, opts, this.deps.now?.());
    return { features, plan, alternatives };
  }

  /** Route and run. Throws only when no model could answer at all. */
  async run(messages: Msg[], opts: RouteOptions, extra: { responseFormatJson?: boolean; signal?: AbortSignal; call?: CallFn } = {}): Promise<RunResult> {
    await this.ensureLoaded();
    const features = classify(messages, extra);
    const models = await this.deps.models();
    const { plan, alternatives } = makePlan(models, features, this.stats, opts, this.deps.now?.());
    const hedgeAt = HEDGE_AT[opts.effort];
    const hedgeAfterMs =
      opts.hedgeAfterMs ?? (hedgeAt && plan.kind !== "fuse" && plan.fallbacks.length ? Math.max(MIN_HEDGE_MS, plan.expected.latencyMs * hedgeAt) : undefined);
    try {
      return await execute({
        messages,
        features,
        plan,
        alternatives,
        models,
        stats: this.stats,
        opts: { ...opts, hedgeAfterMs },
        // Per request (billing goes to the caller), else the default.
        call: extra.call ?? this.deps.call,
        signal: extra.signal,
        now: this.deps.now,
      });
    } finally {
      this.scheduleSave();
    }
  }

  /**
   * Learn from an answer the router did not run itself (a streamed
   * single-model reply passed straight through): verify it and count it.
   */
  observe(model: string, features: Features, text: string, finishReason?: string, call?: { ms: number; outTokens: number }) {
    const v = verify(features, text, finishReason);
    this.stats.recordOutcome(model, features.category, v.pass);
    if (call) this.stats.recordCall(model, { ok: true, ...call });
    this.scheduleSave();
    return v;
  }

  /** Persist now (tests, shutdown). */
  async flush() {
    if (this.saveTimer) clearTimeout(this.saveTimer);
    this.saveTimer = null;
    if (this.deps.store && this.stats.version !== this.savedVersion) {
      this.savedVersion = this.stats.version;
      await this.deps.store.save(this.stats.snapshot());
    }
  }
}
