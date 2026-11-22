// Gallery listings in the site's SQLite Cloud database (SQLITE_CLOUD_URL).
// Written against the two-method Sql interface (lib/chat/store.ts) so tests
// run it on bun:sqlite.
//
// One row per site, keyed by its normalized URL (url_key, unique). The
// editorial seed (GALLERY in lib/gallery.ts) is written once as approved.

import { randomBytes } from "node:crypto";
import type { Sql, Row } from "../chat/store";
import { GALLERY, type GalleryCategory, type GalleryEntry, type ListedVia } from "../gallery";
import { normalizeUrl, slugify } from "./url";

export type ListingStatus =
  /** Free tier, waiting out the queue (or for its badge). */
  | "queued"
  /** Priority tier chosen, CoinPay checkout not paid yet. */
  | "awaiting_payment"
  /** Paid; automated checks run, or held for a person. */
  | "priority_review"
  /** Held for a person (adult label, explicit wording). */
  | "review"
  | "approved"
  | "rejected";

export type Tier = "free" | "priority" | "editorial";
export type BadgeState = "found" | "missing" | "error";

export type Listing = {
  id: string;
  slug: string;
  url: string;
  url_key: string;
  name: string;
  tagline: string;
  category: GalleryCategory;
  built_with: string[];
  c0mpute_url: string | null;
  /** Site-relative screenshot path (seed entries); null = generated card. */
  image: string | null;
  contact_email: string | null;
  tier: Tier;
  status: ListingStatus;
  listed_via: ListedVia | null;
  featured: boolean;
  /** Free tier: when the queue lets it go live without a badge (ISO). */
  goes_live_at: string | null;
  /** YYYY-MM-DD it went live. */
  added_at: string | null;
  /** Editorial order among same-day entries (seed order). */
  sort: number;
  badge_state: BadgeState | null;
  badge_checked_at: string | null;
  badge_missing_since: string | null;
  manual_check_at: string | null;
  check_note: string | null;
  checkout_session_id: string | null;
  paid_at: string | null;
  refund_requested: boolean;
  reject_reason: string | null;
  created_at: string;
  updated_at: string;
  approved_at: string | null;
};

export const GALLERY_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS gallery_listings (
     id                  TEXT PRIMARY KEY,
     slug                TEXT NOT NULL UNIQUE,
     url                 TEXT NOT NULL,
     url_key             TEXT NOT NULL UNIQUE,
     name                TEXT NOT NULL,
     tagline             TEXT NOT NULL DEFAULT '',
     category            TEXT NOT NULL,
     built_with          TEXT NOT NULL DEFAULT '[]',
     c0mpute_url         TEXT,
     image               TEXT,
     contact_email       TEXT,
     tier                TEXT NOT NULL,
     status              TEXT NOT NULL,
     listed_via          TEXT,
     featured            INTEGER NOT NULL DEFAULT 0,
     goes_live_at        TEXT,
     added_at            TEXT,
     sort                INTEGER NOT NULL DEFAULT 0,
     badge_state         TEXT,
     badge_checked_at    TEXT,
     badge_missing_since TEXT,
     manual_check_at     TEXT,
     check_note          TEXT,
     checkout_session_id TEXT,
     paid_at             TEXT,
     refund_requested    INTEGER NOT NULL DEFAULT 0,
     reject_reason       TEXT,
     created_at          TEXT NOT NULL,
     updated_at          TEXT NOT NULL,
     approved_at         TEXT
   )`,
  `CREATE INDEX IF NOT EXISTS idx_gallery_status ON gallery_listings(status, goes_live_at)`,
  `CREATE INDEX IF NOT EXISTS idx_gallery_checkout ON gallery_listings(checkout_session_id)`,
  `CREATE TABLE IF NOT EXISTS gallery_badge_hits (
     url_key     TEXT PRIMARY KEY,
     hits        INTEGER NOT NULL DEFAULT 0,
     last_hit_at TEXT NOT NULL
   )`,
  `CREATE TABLE IF NOT EXISTS gallery_meta (
     k  TEXT PRIMARY KEY,
     v  TEXT NOT NULL
   )`,
];

// Columns any later change adds go here (ALTER TABLE ... ADD COLUMN), each run once.
const MIGRATIONS: string[] = [];

const BOOL = new Set(["featured", "refund_requested"]);
const WRITABLE = new Set<keyof Listing>([
  "slug", "url", "url_key", "name", "tagline", "category", "built_with", "c0mpute_url", "image", "contact_email",
  "tier", "status", "listed_via", "featured", "goes_live_at", "added_at", "sort", "badge_state", "badge_checked_at",
  "badge_missing_since", "manual_check_at", "check_note", "checkout_session_id", "paid_at", "refund_requested",
  "reject_reason", "approved_at",
]);

export type NewListing = {
  url: string;
  name: string;
  tagline: string;
  category: GalleryCategory;
  built_with: string[];
  contact_email: string | null;
  tier: Exclude<Tier, "editorial">;
  status: ListingStatus;
  goes_live_at: string | null;
  c0mpute_url?: string | null;
};

export class DuplicateListing extends Error {
  constructor(readonly existing: Listing) {
    super("already submitted");
  }
}

const nowIso = () => new Date().toISOString();

function rowToListing(r: Row): Listing {
  const l: Record<string, unknown> = { ...r };
  for (const k of BOOL) l[k] = Number(r[k] ?? 0) === 1;
  try {
    l.built_with = JSON.parse(String(r.built_with ?? "[]"));
  } catch {
    l.built_with = [];
  }
  l.sort = Number(r.sort ?? 0);
  return l as Listing;
}

/** A listing as the public feed / page shows it (an approved one). */
export function toEntry(l: Listing): GalleryEntry {
  return {
    slug: l.slug,
    name: l.name,
    tagline: l.tagline,
    url: l.url,
    c0mpute_url: l.c0mpute_url,
    category: l.category,
    built_with: l.built_with,
    image: l.image || `/gallery/card/${l.slug}`,
    added_at: l.added_at ?? l.created_at.slice(0, 10),
    featured: l.featured,
    listed_via: l.listed_via ?? "editorial",
  };
}

export function createGalleryStore(sql: Sql) {
  let ready: Promise<void> | null = null;

  async function init() {
    for (const q of GALLERY_SCHEMA) await sql.run(q);
    for (const m of MIGRATIONS) {
      try {
        await sql.run(m);
      } catch (e) {
        if (!/duplicate column/i.test(String((e as Error)?.message ?? e))) throw e;
      }
    }
    // Seed once: the meta row's primary key makes "once" atomic across processes,
    // and an editor deleting a seed entry later does not bring it back.
    let first = false;
    try {
      await sql.run(`INSERT INTO gallery_meta (k, v) VALUES ('seed_v1', ?)`, [nowIso()]);
      first = true;
    } catch (e) {
      if (!/unique|constraint|primary key/i.test(String((e as Error)?.message ?? e))) throw e;
    }
    if (first) {
      const t = nowIso();
      for (const [i, e] of GALLERY.entries()) {
        const n = normalizeUrl(e.url);
        if (!n) continue;
        await sql.run(
          `INSERT OR IGNORE INTO gallery_listings
             (id, slug, url, url_key, name, tagline, category, built_with, c0mpute_url, image, tier, status, listed_via,
              featured, added_at, sort, created_at, updated_at, approved_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'editorial', 'approved', 'editorial', 0, ?, ?, ?, ?, ?)`,
          [`seed-${e.slug}`, e.slug, e.url, n.key, e.name, e.tagline, e.category, JSON.stringify(e.built_with), e.c0mpute_url, e.image, e.added_at, i, t, t, t],
        );
      }
    }
  }
  const ensure = () => (ready ??= init().catch((e) => {
    ready = null;
    throw e;
  }));

  async function one(q: string, p: unknown[]): Promise<Listing | null> {
    await ensure();
    const rows = await sql.all(q, p);
    return rows[0] ? rowToListing(rows[0]) : null;
  }
  async function many(q: string, p: unknown[] = []): Promise<Listing[]> {
    await ensure();
    return (await sql.all(q, p)).map(rowToListing);
  }

  async function uniqueSlug(base: string): Promise<string> {
    const b = slugify(base);
    for (let i = 1; i < 50; i++) {
      const s = i === 1 ? b : `${b}-${i}`;
      if (!(await one(`SELECT * FROM gallery_listings WHERE slug = ?`, [s]))) return s;
    }
    return `${b}-${randomBytes(3).toString("hex")}`;
  }

  const store = {
    ensure,
    byId: (id: string) => one(`SELECT * FROM gallery_listings WHERE id = ?`, [id]),
    bySlug: (slug: string) => one(`SELECT * FROM gallery_listings WHERE slug = ?`, [slug]),
    byKey: (key: string) => one(`SELECT * FROM gallery_listings WHERE url_key = ?`, [key]),
    byCheckoutSession: (cs: string) => one(`SELECT * FROM gallery_listings WHERE checkout_session_id = ?`, [cs]),

    /** Approved listings, newest first (seed order breaks same-day ties). */
    approved: () =>
      many(`SELECT * FROM gallery_listings WHERE status = 'approved' ORDER BY added_at DESC, sort ASC, approved_at DESC`),

    list: (o: { status?: ListingStatus | "all"; limit?: number } = {}) =>
      o.status && o.status !== "all"
        ? many(`SELECT * FROM gallery_listings WHERE status = ? ORDER BY updated_at DESC LIMIT ?`, [o.status, o.limit ?? 200])
        : many(`SELECT * FROM gallery_listings ORDER BY updated_at DESC LIMIT ?`, [o.limit ?? 200]),

    async create(n: NewListing): Promise<Listing> {
      const norm = normalizeUrl(n.url);
      if (!norm) throw new Error("invalid url");
      const existing = await store.byKey(norm.key);
      if (existing) throw new DuplicateListing(existing);
      const t = nowIso();
      const id = `gl_${randomBytes(12).toString("base64url")}`;
      const slug = await uniqueSlug(n.name || norm.key.split("/")[0]);
      try {
        await sql.run(
          `INSERT INTO gallery_listings
             (id, slug, url, url_key, name, tagline, category, built_with, c0mpute_url, contact_email, tier, status,
              goes_live_at, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
          [id, slug, norm.url, norm.key, n.name, n.tagline, n.category, JSON.stringify(n.built_with), n.c0mpute_url ?? null,
            n.contact_email, n.tier, n.status, n.goes_live_at, t, t],
        );
      } catch (e) {
        // Two submissions of the same site racing: the unique url_key settles it.
        const dup = await store.byKey(norm.key);
        if (dup) throw new DuplicateListing(dup);
        throw e;
      }
      return (await store.byId(id))!;
    },

    async update(id: string, patch: Partial<Listing>): Promise<Listing | null> {
      const keys = Object.keys(patch).filter((k) => WRITABLE.has(k as keyof Listing)) as (keyof Listing)[];
      if (keys.length) {
        await ensure();
        const vals = keys.map((k) => {
          const v = patch[k];
          if (k === "built_with") return JSON.stringify(v ?? []);
          if (BOOL.has(k)) return v ? 1 : 0;
          return v ?? null;
        });
        await sql.run(`UPDATE gallery_listings SET ${keys.map((k) => `${k} = ?`).join(", ")}, updated_at = ? WHERE id = ?`, [...vals, nowIso(), id]);
      }
      return store.byId(id);
    },

    async remove(id: string): Promise<void> {
      await ensure();
      await sql.run(`DELETE FROM gallery_listings WHERE id = ?`, [id]);
    },

    /** Free listings whose queue time is up. */
    dueQueued: (now: string, limit: number) =>
      many(`SELECT * FROM gallery_listings WHERE status = 'queued' AND goes_live_at IS NOT NULL AND goes_live_at <= ? ORDER BY goes_live_at LIMIT ?`, [now, limit]),

    /** Queued listings not scanned for a badge since `before`. */
    queuedForBadgeScan: (before: string, limit: number) =>
      many(
        `SELECT * FROM gallery_listings WHERE status = 'queued' AND (badge_checked_at IS NULL OR badge_checked_at < ?) ORDER BY badge_checked_at IS NOT NULL, badge_checked_at LIMIT ?`,
        [before, limit],
      ),

    /** Listings live on the badge that are due a re-check: weekly, or after the grace period once missing. */
    badgeRecheckDue: (weekBefore: string, graceBefore: string, limit: number) =>
      many(
        `SELECT * FROM gallery_listings WHERE status = 'approved' AND listed_via = 'badge'
           AND ((badge_missing_since IS NULL AND (badge_checked_at IS NULL OR badge_checked_at < ?))
             OR (badge_missing_since IS NOT NULL AND badge_checked_at < ?))
         ORDER BY badge_checked_at LIMIT ?`,
        [weekBefore, graceBefore, limit],
      ),

    /** Badge loads, by listing URL: { url_key: n } added to the stored counts. */
    async addHits(counts: Map<string, number>): Promise<void> {
      if (!counts.size) return;
      await ensure();
      const t = nowIso();
      for (const [key, n] of counts) {
        await sql.run(
          `INSERT INTO gallery_badge_hits (url_key, hits, last_hit_at) VALUES (?, ?, ?)
           ON CONFLICT(url_key) DO UPDATE SET hits = hits + excluded.hits, last_hit_at = excluded.last_hit_at`,
          [key, n, t],
        );
      }
    },

    async hits(key: string): Promise<number> {
      await ensure();
      const r = await sql.all(`SELECT hits FROM gallery_badge_hits WHERE url_key = ?`, [key]);
      return Number(r[0]?.hits ?? 0);
    },
  };
  return store;
}

export type GalleryStore = ReturnType<typeof createGalleryStore>;
