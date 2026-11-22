// "Get listed" on the c0mpute gallery: submit, badge checks, the 30-day free
// queue, priority checkout (CoinPay) and moderation. The web routes, the MCP
// tools and the CLI (through the API) all call this, so there is one set of
// rules. Everything outside (database, fetch, reputation, mail, CoinPay, the
// clock) is injected, so tests drive it end to end.

import { GALLERY_CATEGORIES, type GalleryCategory, type ListedVia } from "../gallery";
import { badgeMarkdown, badgeSnippet } from "./badge";
import { GALLERY_CONFIG, GALLERY_ORIGIN } from "./config";
import { renderMail, statusUrl, type Mailer, type MailKind } from "./email";
import { DuplicateListing, type GalleryStore, type Listing, type ListingStatus } from "./store";
import { isGatewayUrl, normalizeUrl } from "./url";
import { checkBadge, runChecks, type BadgeResult, type Checks, type ReputationLookup, type SafeFetchDeps } from "./verify";

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

export type GalleryDeps = {
  store: GalleryStore;
  fetchDeps?: SafeFetchDeps;
  reputation?: ReputationLookup;
  mailer?: Mailer;
  checkout?: CheckoutOpener;
  now?: () => Date;
  log?: (msg: string) => void;
};

export class GalleryError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly listing?: Listing,
  ) {
    super(message);
  }
}

export type Submission = {
  url: string;
  name: string;
  tagline: string;
  category: GalleryCategory;
  built_with: string[];
  contact_email: string | null;
  tier: "free" | "priority";
};

const EMAIL = /^[^\s@<>()"',;:]{1,64}@[A-Za-z0-9.-]{1,190}\.[A-Za-z]{2,}$/;
// Control characters, bidi overrides and zero-width characters out of display text.
const clean = (s: unknown, max: number) =>
  typeof s === "string" ? s.replace(/[\u0000-\u001f\u007f​-‏‪-‮⁦-⁩]/g, " ").replace(/\s+/g, " ").trim().slice(0, max) : "";

/** Validate a submission from a form, JSON, the CLI or MCP. A string is the error. */
export function parseSubmission(body: unknown): Submission | string {
  const b = (body && typeof body === "object" ? body : {}) as Record<string, unknown>;
  const norm = normalizeUrl(b.url);
  if (!norm) return "url: give the public http(s) address of your site (a domain like example.com works; no IPs, ports or logins)";
  const tier = b.tier === "priority" ? "priority" : b.tier === undefined || b.tier === "" || b.tier === "free" ? "free" : null;
  if (!tier) return "tier: free or priority";
  const category = (typeof b.category === "string" && b.category ? b.category : "apps") as GalleryCategory;
  if (!GALLERY_CATEGORIES.some((c) => c.id === category)) return `category: one of ${GALLERY_CATEGORIES.map((c) => c.id).join(", ")}`;
  const rawBuilt = Array.isArray(b.built_with) ? b.built_with : typeof b.built_with === "string" ? b.built_with.split(",") : [];
  const built_with = [...new Set(rawBuilt.map((x) => clean(x, 40)).filter(Boolean))].slice(0, 6);
  const email = clean(b.contact_email ?? b.email, 254).toLowerCase();
  if (email && !EMAIL.test(email)) return "contact_email: that does not look like an email address";
  if (tier === "priority" && !email) return "contact_email: priority review needs an email for the CoinPay receipt";
  const name = clean(b.name, 60) || norm.host.replace(/^www\./, "");
  return { url: norm.url, name, tagline: clean(b.tagline, 200), category, built_with, contact_email: email || null, tier };
}

const isoDay = (d: Date) => d.toISOString().slice(0, 10);
const plusDays = (d: Date, n: number) => new Date(d.getTime() + n * 86_400_000);
const plusHours = (d: Date, n: number) => new Date(d.getTime() + n * 3_600_000);

/** What anyone may see about a listing (no contact email, no payment ids). */
export function publicStatus(l: Listing, hits?: number) {
  return {
    object: "gallery.listing" as const,
    id: l.id,
    slug: l.slug,
    url: l.url,
    name: l.name,
    tagline: l.tagline,
    category: l.category,
    built_with: l.built_with,
    tier: l.tier,
    status: l.status,
    listed_via: l.listed_via,
    featured: l.featured,
    goes_live_at: l.status === "queued" ? l.goes_live_at : null,
    added_at: l.status === "approved" ? l.added_at : null,
    paid: !!l.paid_at,
    badge: { state: l.badge_state, checked_at: l.badge_checked_at, missing_since: l.badge_missing_since, ...(hits !== undefined ? { hits } : {}) },
    note: l.status === "rejected" ? l.reject_reason : l.check_note,
    status_url: statusUrl(l),
    gallery_url: l.status === "approved" ? `${GALLERY_ORIGIN}/gallery#${l.slug}` : null,
    badge_svg: `${GALLERY_ORIGIN}/badge.svg?url=${encodeURIComponent(l.url)}`,
    snippet: badgeSnippet(l.url),
    markdown: badgeMarkdown(l.url),
    priority_price_usd: GALLERY_CONFIG.priorityPriceUsd,
  };
}

/** Everything the moderation view shows (adds the contact and payment fields). */
export function adminView(l: Listing) {
  return {
    ...publicStatus(l),
    contact_email: l.contact_email,
    checkout_session_id: l.checkout_session_id,
    paid_at: l.paid_at,
    refund_requested: l.refund_requested,
    reject_reason: l.reject_reason,
    created_at: l.created_at,
    updated_at: l.updated_at,
  };
}

export type AdminAction = "approve" | "reject" | "feature" | "unfeature" | "requeue" | "delete";
export const ADMIN_ACTIONS: AdminAction[] = ["approve", "reject", "feature", "unfeature", "requeue", "delete"];

export function createGalleryService(deps: GalleryDeps) {
  const { store } = deps;
  const now = () => (deps.now ? deps.now() : new Date());
  const log = deps.log ?? ((m: string) => console.log(`[gallery] ${m}`));
  const checksDeps = { ...deps.fetchDeps, reputation: deps.reputation };
  const nudged = new Map<string, number>();

  async function mail(kind: MailKind, l: Listing | null) {
    if (!l || !deps.mailer) return;
    try {
      const m = await renderMail(kind, l);
      if (m) await deps.mailer(m);
    } catch (e) {
      log(`mail ${kind} for ${l.id} failed: ${e instanceof Error ? e.message : e}`);
    }
  }

  async function approve(l: Listing, via: ListedVia, patch: Partial<Listing> = {}): Promise<Listing> {
    const t = now();
    const done = await store.update(l.id, {
      ...patch,
      status: "approved",
      listed_via: via,
      added_at: isoDay(t),
      approved_at: t.toISOString(),
      goes_live_at: null,
      reject_reason: null,
      ...(via === "badge" ? { tier: l.paid_at ? l.tier : "free", badge_missing_since: null } : {}),
    });
    log(`approved ${l.url} via ${via}`);
    await mail("approved", done);
    return done!;
  }

  async function openCheckout(l: Listing): Promise<{ url: string; id: string }> {
    if (!deps.checkout) throw new GalleryError("priority checkout is not configured on this server", 503, l);
    if (l.status === "approved") throw new GalleryError("already listed", 409, l);
    if (l.status === "rejected") throw new GalleryError("this submission was not accepted", 409, l);
    if (l.paid_at) throw new GalleryError("already paid: priority review is under way", 409, l);
    if (!l.contact_email) throw new GalleryError("priority review needs a contact email for the CoinPay receipt: resubmit with one", 400, l);
    const s = await deps.checkout({
      amount: GALLERY_CONFIG.priorityPriceUsd,
      description: `c0mpute gallery: priority listing for ${l.name} (${l.url})`,
      customerEmail: l.contact_email,
      accountId: l.id,
      metadata: { kind: "gallery_priority", listing_id: l.id, url: l.url },
      successUrl: `${statusUrl(l)}?paid=1`,
      cancelUrl: statusUrl(l),
    });
    await store.update(l.id, { checkout_session_id: s.id });
    return s;
  }

  function badgePatch(b: BadgeResult, t: Date): Partial<Listing> {
    return { badge_state: b.state, badge_checked_at: t.toISOString(), check_note: b.note };
  }

  /** A queued listing with its badge in place: run the checks and put it live. */
  async function approveOnBadge(l: Listing, b: BadgeResult, checks?: Checks): Promise<Listing> {
    const c = checks ?? (await runChecks(l.url, l, checksDeps));
    if (!c.ok) {
      return (await store.update(l.id, { ...badgePatch(b, now()), status: c.hold ? "review" : l.status, check_note: c.notes.join("; ") || b.note }))!;
    }
    return approve(l, "badge", badgePatch(b, now()));
  }

  const service = {
    store,

    async submit(input: Submission): Promise<{ listing: Listing; created: boolean; checkout_url: string | null; message: string }> {
      const existing = await store.byKey(normalizeUrl(input.url)!.key);
      if (existing) throw new GalleryError("this site is already submitted", 409, existing);
      const checks = await runChecks(input.url, input, checksDeps);
      if (checks.refuse) throw new GalleryError(checks.notes.join("; ") || "this site cannot be listed", 422);
      const badge = await checkBadge(input.url, deps.fetchDeps, checks.page);
      const t = now();
      let status: ListingStatus;
      if (checks.hold) status = input.tier === "priority" ? "awaiting_payment" : "review";
      else if (badge.state === "found") status = "queued"; // approved just below, through the one approve path
      else status = input.tier === "priority" ? "awaiting_payment" : "queued";
      let listing: Listing;
      try {
        listing = await store.create({
          ...input,
          tier: badge.state === "found" && !checks.hold ? "free" : input.tier,
          status,
          goes_live_at: status === "queued" ? plusDays(t, GALLERY_CONFIG.freeQueueDays).toISOString() : null,
          c0mpute_url: isGatewayUrl(input.url) ? input.url : null,
        });
      } catch (e) {
        if (e instanceof DuplicateListing) throw new GalleryError("this site is already submitted", 409, e.existing);
        throw e;
      }
      listing = (await store.update(listing.id, { ...badgePatch(badge, t), check_note: [...checks.notes, badge.note].join("; ") }))!;
      log(`submitted ${listing.url} (${listing.tier}, ${listing.status}, badge ${badge.state})`);

      if (badge.state === "found" && !checks.hold) {
        listing = await approve(listing, "badge");
        return { listing, created: true, checkout_url: null, message: "badge found: listed now" };
      }
      let checkout_url: string | null = null;
      let message =
        status === "review"
          ? "received: a person will look at it before it is listed"
          : status === "queued"
            ? `queued: goes live on ${listing.goes_live_at!.slice(0, 10)}, or as soon as we find the badge on your site`
            : `pay $${GALLERY_CONFIG.priorityPriceUsd} to finish priority review`;
      if (status === "awaiting_payment") {
        try {
          checkout_url = (await openCheckout(listing)).url;
        } catch (e) {
          message = `saved, but checkout could not open (${e instanceof Error ? e.message : e}): open it from the status page`;
        }
        listing = (await store.byId(listing.id))!;
      }
      await mail("submitted", listing);
      return { listing, created: true, checkout_url, message };
    },

    openCheckout,

    /** "Check my badge now" (manual) or a background look. */
    async checkNow(l: Listing, o: { manual?: boolean } = {}): Promise<{ listing: Listing; badge: BadgeResult; message: string }> {
      const t = now();
      if (o.manual && l.manual_check_at && t.getTime() - Date.parse(l.manual_check_at) < GALLERY_CONFIG.manualCheckCooldownSeconds * 1000) {
        throw new GalleryError(`checked a moment ago: try again in a minute`, 429, l);
      }
      if (l.status === "rejected") throw new GalleryError("this submission was not accepted", 409, l);
      if (o.manual) await store.update(l.id, { manual_check_at: t.toISOString() });
      const badge = await checkBadge(l.url, deps.fetchDeps);
      if (badge.state === "found" && (l.status === "queued" || l.status === "awaiting_payment")) {
        const listing = await approveOnBadge(l, badge);
        return { listing, badge, message: listing.status === "approved" ? "badge found: you are listed now" : `badge found, but: ${listing.check_note}` };
      }
      const patch: Partial<Listing> = badgePatch(badge, t);
      if (badge.state === "found") patch.badge_missing_since = null;
      const listing = (await store.update(l.id, patch))!;
      const message =
        badge.state === "found"
          ? "badge found"
          : badge.state === "missing"
            ? `no badge yet on ${badge.checked.join(" or ")}: paste the snippet into a page we can fetch (the URL you listed, or its homepage)`
            : `could not check: ${badge.note}`;
      return { listing, badge, message };
    },

    /** A badge was loaded for this URL key: if its listing is queued, look now (at most every 10 minutes). */
    async nudge(key: string): Promise<void> {
      const t = Date.now();
      if ((nudged.get(key) ?? 0) > t - 600_000) return;
      nudged.set(key, t);
      if (nudged.size > 5000) nudged.clear();
      const l = await store.byKey(key);
      if (l && l.status === "queued") await service.checkNow(l).catch(() => {});
    },

    /** A verified CoinPay webhook: settle a paid priority listing. */
    async onPaid(e: PaidEvent): Promise<{ listing: Listing | null; action: string }> {
      if (!e.paid) return { listing: null, action: "ignored: not a payment" };
      if (e.metadata.kind !== "gallery_priority") return { listing: null, action: "ignored: not a gallery payment" };
      const id = (typeof e.metadata.listing_id === "string" && e.metadata.listing_id) || e.accountId || "";
      let l = id ? await store.byId(id) : null;
      if (!l && e.checkoutSessionId) l = await store.byCheckoutSession(e.checkoutSessionId);
      if (!l) return { listing: null, action: "ignored: unknown listing" };
      if (l.paid_at) return { listing: l, action: "already settled" };
      const t = now();
      const short = e.amountUsd > 0 && e.amountUsd < GALLERY_CONFIG.priorityPriceUsd - 0.5;
      l = (await store.update(l.id, {
        paid_at: t.toISOString(),
        tier: "priority",
        checkout_session_id: e.checkoutSessionId ?? l.checkout_session_id,
        ...(l.status === "approved" ? {} : { status: "priority_review" as const, goes_live_at: null }),
      }))!;
      log(`paid priority for ${l.url}`);
      if (l.status === "approved") return { listing: l, action: "paid (was already listed)" };
      if (short) {
        l = (await store.update(l.id, { check_note: `paid $${e.amountUsd}, less than $${GALLERY_CONFIG.priorityPriceUsd}: a person will look` }))!;
        return { listing: l, action: "held: underpaid" };
      }
      const c = await runChecks(l.url, l, checksDeps);
      if (!c.ok) {
        l = (await store.update(l.id, { check_note: `held for a person: ${c.notes.join("; ")}` }))!;
        return { listing: l, action: "held for review" };
      }
      l = await approve(l, "priority", { check_note: c.notes.join("; ") || null });
      return { listing: l, action: "approved" };
    },

    async admin(action: AdminAction, l: Listing, o: { reason?: string; refund?: boolean } = {}): Promise<Listing | null> {
      switch (action) {
        case "approve":
          return approve(l, l.paid_at ? "priority" : "admin");
        case "reject": {
          const done = await store.update(l.id, {
            status: "rejected",
            reject_reason: clean(o.reason, 300) || null,
            refund_requested: !!(o.refund && l.paid_at),
            goes_live_at: null,
          });
          await mail("rejected", done);
          return done;
        }
        case "feature":
        case "unfeature":
          return store.update(l.id, { featured: action === "feature" });
        case "requeue":
          return store.update(l.id, {
            status: "queued",
            listed_via: null,
            added_at: null,
            approved_at: null,
            goes_live_at: plusDays(now(), GALLERY_CONFIG.freeQueueDays).toISOString(),
          });
        case "delete":
          await store.remove(l.id);
          return null;
      }
    },

    /** One background pass: queue promotion, badge scans of queued listings, weekly re-checks. */
    async tick(): Promise<{ promoted: number; badgeApproved: number; rechecked: number; requeued: number }> {
      const out = { promoted: 0, badgeApproved: 0, rechecked: 0, requeued: 0 };
      const t = now();
      const n = GALLERY_CONFIG.tickBatch;

      for (const l of await store.dueQueued(t.toISOString(), n)) {
        const c = await runChecks(l.url, l, checksDeps);
        if (c.ok) {
          await approve(l, "queue", { check_note: c.notes.join("; ") || null });
          out.promoted++;
        } else {
          await store.update(l.id, { status: "review", check_note: `queue done, but: ${c.notes.join("; ")}` });
        }
      }

      for (const l of await store.queuedForBadgeScan(plusHours(t, -GALLERY_CONFIG.queuedBadgeScanHours).toISOString(), n)) {
        const b = await checkBadge(l.url, deps.fetchDeps);
        if (b.state === "found") {
          const done = await approveOnBadge(l, b);
          if (done.status === "approved") out.badgeApproved++;
        } else {
          await store.update(l.id, badgePatch(b, t));
        }
      }

      const week = plusDays(t, -GALLERY_CONFIG.badgeRecheckDays).toISOString();
      const grace = plusHours(t, -GALLERY_CONFIG.badgeMissingGraceHours).toISOString();
      for (const l of await store.badgeRecheckDue(week, grace, n)) {
        out.rechecked++;
        const b = await checkBadge(l.url, deps.fetchDeps);
        if (b.state === "found") {
          await store.update(l.id, { ...badgePatch(b, t), badge_missing_since: null });
        } else if (b.state === "error") {
          await store.update(l.id, badgePatch(b, t)); // could not tell: try again next pass
        } else if (!l.badge_missing_since) {
          await store.update(l.id, { ...badgePatch(b, t), badge_missing_since: t.toISOString() });
        } else {
          const done = await store.update(l.id, {
            ...badgePatch(b, t),
            status: "queued",
            listed_via: null,
            added_at: null,
            approved_at: null,
            goes_live_at: plusDays(t, GALLERY_CONFIG.freeQueueDays).toISOString(),
          });
          log(`badge gone from ${l.url}: back in the queue`);
          await mail("badge_missing", done);
          out.requeued++;
        }
      }
      return out;
    },
  };
  return service;
}

export type GalleryService = ReturnType<typeof createGalleryService>;
