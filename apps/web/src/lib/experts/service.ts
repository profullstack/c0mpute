// The Experts store's rules: who may ask which expert, the one free trial per
// CoinPay account, the paywall after it, CoinPay checkout and its webhook.
// Pure over its deps (store, checkout, clock) so tests run it on bun:sqlite.

import { expertsConfig, expertPriceUsd, type ExpertsConfig } from "./config";
import type { ExpertManifest } from "./manifest";
import type { ExpertsStore, Subscription, Trial } from "./store";

export type CheckoutOpener = (o: {
  amount: number;
  description: string;
  customerEmail: string;
  accountId: string;
  metadata: Record<string, unknown>;
  successUrl: string;
  cancelUrl: string;
}) => Promise<{ id: string; url: string }>;

export type PaidEvent = {
  paid: boolean;
  checkoutSessionId: string | null;
  accountId: string | null;
  amountUsd: number;
  metadata: Record<string, unknown>;
};

/** Who is asking: the signed-in c0mpute account. Bound by CoinPay sub, never by email. */
export type Asker = { id: string; coinpay_sub: string | null };

export type AccessState =
  /** Inside the free trial. */
  | "trial"
  /** A paid plan covers this expert. */
  | "subscribed"
  /** Has CoinPay, never started the trial: the first question starts it. */
  | "trial_available"
  /** Trial used up and nothing paid: paywall. */
  | "expired"
  /** Passkey/password-only account: connect CoinPay first. */
  | "no_coinpay";

export type Access = {
  allowed: boolean;
  state: AccessState;
  trial: { started_at: string; ends_at: string; days_left: number } | null;
  /** The plan that covers this expert ("all" or the slug), and until when. */
  plan: { plan: string; paid_until: string } | null;
};

export const BUNDLE = "all";

export class ExpertsError extends Error {
  constructor(
    message: string,
    public status: number,
    public code: string,
    public extra: Record<string, unknown> = {},
  ) {
    super(message);
  }
}

const DAY = 86_400_000;

export type ExpertsDeps = {
  store: ExpertsStore;
  catalog: () => ExpertManifest[];
  checkout?: CheckoutOpener;
  site?: string;
  now?: () => Date;
  config?: () => ExpertsConfig;
  log?: (m: string) => void;
};

export function createExpertsService(deps: ExpertsDeps) {
  const now = deps.now ?? (() => new Date());
  const cfg = deps.config ?? (() => expertsConfig());
  const log = deps.log ?? ((m: string) => console.log(`[experts] ${m}`));
  const site = (deps.site ?? "https://c0mpute.com").replace(/\/$/, "");
  const { store } = deps;

  const trialView = (t: Trial | null, at: Date) =>
    t ? { started_at: t.started_at, ends_at: t.ends_at, days_left: Math.max(0, Math.ceil((Date.parse(t.ends_at) - at.getTime()) / DAY)) } : null;

  const covering = (subs: Subscription[], slug: string, at: Date) =>
    subs
      .filter((s) => (s.plan === BUNDLE || s.plan === slug) && Date.parse(s.paid_until) > at.getTime())
      .sort((a, b) => Date.parse(b.paid_until) - Date.parse(a.paid_until))[0] ?? null;

  /** Plan ids a checkout may buy: "all" or an expert's slug. */
  const planPrice = (plan: string): { amount: number; label: string } | null => {
    const c = cfg();
    if (plan === BUNDLE) return { amount: c.bundlePriceUsd, label: "all experts" };
    const m = deps.catalog().find((x) => x.slug === plan);
    return m ? { amount: expertPriceUsd(m.pricing.monthly_usd, c), label: m.name } : null;
  };

  const service = {
    planPrice,

    async access(user: Asker, slug: string): Promise<Access> {
      const at = now();
      if (!user.coinpay_sub) return { allowed: false, state: "no_coinpay", trial: null, plan: null };
      const [trial, subs] = await Promise.all([store.trial(user.coinpay_sub), store.subscriptions(user.coinpay_sub)]);
      const t = trialView(trial, at);
      const p = covering(subs, slug, at);
      if (p) return { allowed: true, state: "subscribed", trial: t, plan: { plan: p.plan, paid_until: p.paid_until } };
      if (trial && Date.parse(trial.ends_at) > at.getTime()) return { allowed: true, state: "trial", trial: t, plan: null };
      if (trial) return { allowed: false, state: "expired", trial: t, plan: null };
      return { allowed: false, state: "trial_available", trial: null, plan: null };
    },

    /** Start this account's one free trial (idempotent: a second call returns the same trial). */
    async startTrial(user: Asker): Promise<{ trial: NonNullable<Access["trial"]>; created: boolean }> {
      if (!user.coinpay_sub) throw new ExpertsError("Connect CoinPay to start the free trial: one trial per CoinPay account.", 403, "coinpay_required", { connect: `${site}/api/auth/coinpay/login?link=1&next=/experts` });
      const at = now();
      const r = await store.startTrial(user.coinpay_sub, user.id, cfg().trialDays, at);
      if (r.created) log(`trial started for ${user.id} (${cfg().trialDays} days)`);
      return { trial: trialView(r.trial, at)!, created: r.created };
    },

    /**
     * The gate every question passes: allowed (trial or plan), or the first
     * question starts the trial, or an ExpertsError (402 paywall / 403 CoinPay).
     */
    async gate(user: Asker, m: ExpertManifest): Promise<Access> {
      let a = await service.access(user, m.slug);
      if (a.state === "trial_available") {
        await service.startTrial(user);
        a = await service.access(user, m.slug);
      }
      if (a.allowed) {
        store.countAsk(user.coinpay_sub!, m.slug, now()).catch(() => {});
        return a;
      }
      if (a.state === "no_coinpay")
        throw new ExpertsError("Experts need a CoinPay-connected account (one free trial per CoinPay account). Connect CoinPay on c0mpute.com/account.", 403, "coinpay_required", {
          connect: `${site}/api/auth/coinpay/login?link=1&next=/experts/${m.slug}`,
        });
      const per = planPrice(m.slug)!;
      throw new ExpertsError(
        `Your ${cfg().trialDays}-day free trial ended on ${a.trial?.ends_at.slice(0, 10)}. Subscribe to keep asking ${m.name}: $${per.amount}/${cfg().periodDays} days, or every expert for $${cfg().bundlePriceUsd}.`,
        402,
        "trial_ended",
        { subscribe: `${site}/experts/${m.slug}#subscribe`, plans: [{ plan: m.slug, price_usd: per.amount }, { plan: BUNDLE, price_usd: cfg().bundlePriceUsd }] },
      );
    },

    /** The account's whole Experts picture (account page, /api/v1/experts/me, CLI). */
    async status(user: Asker) {
      const c = cfg();
      const at = now();
      const base = {
        coinpay: !!user.coinpay_sub,
        prices: { per_expert_usd: Object.fromEntries(deps.catalog().map((m) => [m.slug, expertPriceUsd(m.pricing.monthly_usd, c)])), all_usd: c.bundlePriceUsd, period_days: c.periodDays, trial_days: c.trialDays },
      };
      if (!user.coinpay_sub) return { ...base, trial: null, trial_available: false, subscriptions: [], usage: {}, experts: deps.catalog().map((m) => ({ slug: m.slug, state: "no_coinpay" as AccessState, allowed: false })) };
      const [trial, subs, usage] = await Promise.all([store.trial(user.coinpay_sub), store.subscriptions(user.coinpay_sub), store.usage(user.coinpay_sub)]);
      const experts = await Promise.all(deps.catalog().map(async (m) => ({ slug: m.slug, ...(await service.access(user, m.slug)) })));
      return {
        ...base,
        trial: trialView(trial, at),
        trial_available: !trial,
        subscriptions: subs.map((s) => ({ plan: s.plan, paid_until: s.paid_until, active: Date.parse(s.paid_until) > at.getTime() })),
        usage,
        experts: experts.map((e) => ({ slug: e.slug, state: e.state, allowed: e.allowed })),
      };
    },

    /** Open a CoinPay checkout for one period of `plan` ("all" or a slug). */
    async checkout(user: Asker, plan: string, email: string): Promise<{ url: string; checkout_id: string; amount_usd: number; plan: string }> {
      if (!user.coinpay_sub) throw new ExpertsError("Connect CoinPay first: subscriptions are bound to your CoinPay account.", 403, "coinpay_required");
      const price = planPrice(plan);
      if (!price) throw new ExpertsError(`unknown plan "${plan}": use "all" or an expert slug`, 400, "bad_plan");
      if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email) || email.length > 254) throw new ExpertsError("a receipt email is required for CoinPay checkout", 400, "email_required");
      if (!deps.checkout) throw new ExpertsError("checkout is not configured on this server", 503, "checkout_unavailable");
      const c = cfg();
      const at = now();
      const row = await store.createCheckout({ coinpay_sub: user.coinpay_sub, user_id: user.id, plan, amount_usd: price.amount, period_days: c.periodDays }, at);
      const back = plan === BUNDLE ? `${site}/experts` : `${site}/experts/${plan}`;
      const s = await deps.checkout({
        amount: price.amount,
        description: `c0mpute Experts: ${price.label}, ${c.periodDays} days`,
        customerEmail: email,
        accountId: user.coinpay_sub,
        metadata: { kind: "experts_subscription", checkout_id: row.id, plan, period_days: c.periodDays },
        successUrl: `${back}?paid=1`,
        cancelUrl: back,
      });
      await store.setCheckoutSession(row.id, s.id);
      log(`checkout ${row.id} opened: ${plan} $${price.amount}`);
      return { url: s.url, checkout_id: row.id, amount_usd: price.amount, plan };
    },

    /** A verified CoinPay webhook. Ignores anything that is not an Experts payment. */
    async onPaid(e: PaidEvent): Promise<{ handled: boolean; action: string }> {
      if (e.metadata.kind !== "experts_subscription") return { handled: false, action: "not an experts payment" };
      if (!e.paid) return { handled: true, action: "ignored: not a payment" };
      const id = typeof e.metadata.checkout_id === "string" ? e.metadata.checkout_id : "";
      let row = id ? await store.checkout(id) : null;
      if (!row && e.checkoutSessionId) row = await store.checkoutBySession(e.checkoutSessionId);
      if (!row) return { handled: true, action: "ignored: unknown checkout" };
      if (row.paid_at) return { handled: true, action: "already settled" };
      if (e.amountUsd > 0 && e.amountUsd < row.amount_usd - 0.5) {
        log(`checkout ${row.id} underpaid: $${e.amountUsd} of $${row.amount_usd}; not extended`);
        return { handled: true, action: "held: underpaid" };
      }
      const at = now();
      if (!(await store.markPaid(row.id, at))) return { handled: true, action: "already settled" };
      const s = await store.extend(row.coinpay_sub, row.plan, row.user_id, row.period_days, at);
      log(`checkout ${row.id} paid: ${row.plan} until ${s.paid_until}`);
      return { handled: true, action: `extended ${row.plan} until ${s.paid_until}` };
    },
  };
  return service;
}

export type ExpertsService = ReturnType<typeof createExpertsService>;
