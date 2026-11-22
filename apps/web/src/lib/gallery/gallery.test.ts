import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { signWebhook, verifyWebhook } from "@profullstack/coinpay-onboard";
import type { Sql } from "../chat/store";
import { handleRpc } from "../chat/mcp";
import { GALLERY, GALLERY_SCHEMA_VERSION, galleryResponse } from "../gallery";
import { badgeKey, badgeSnippet, badgeSvg, countHit, takeHits } from "./badge";
import { GALLERY_CONFIG } from "./config";
import { mailBody, type GalleryMail } from "./email";
import { createGalleryService, GalleryError, parseSubmission, publicStatus, type Submission } from "./service";
import { createGalleryStore, toEntry } from "./store";
import { normalizeUrl } from "./url";
import { checkBadge, FetchRefused, hasBadge, isPrivateAddress, safeFetch } from "./verify";

process.env.CROSS_PROMO = "bundled"; // never fetch the catalog in tests

function memSql(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}

type Page = { status?: number; html?: string; location?: string };
/** A fake web: url -> page. Unknown URLs are 404. */
function web(pages: Record<string, Page>) {
  const seen: string[] = [];
  const f = (async (input: string | URL | Request) => {
    const u = String(input).replace(/\/$/, "");
    seen.push(u);
    const p = pages[u] ?? pages[`${u}/`] ?? { status: 404, html: "nope" };
    const headers = new Headers({ "content-type": "text/html" });
    if (p.location) headers.set("location", p.location);
    return new Response(p.html ?? "", { status: p.status ?? 200, headers });
  }) as typeof fetch;
  return { fetch: f, resolve: async () => ["93.184.216.34"], seen };
}

const BADGE_HTML = `<html><body><footer>${badgeSnippet("https://example.com")}</footer></body></html>`;
const clean = async () => ({ listed: false, reasons: [] });

function setup(pages: Record<string, Page>, o: { at?: string } = {}) {
  const w = web(pages);
  const mails: GalleryMail[] = [];
  const sessions: string[] = [];
  let clock = new Date(o.at ?? "2026-11-22T12:00:00Z");
  const store = createGalleryStore(memSql());
  const g = createGalleryService({
    store,
    fetchDeps: { fetch: w.fetch, resolve: w.resolve },
    reputation: clean,
    mailer: async (m) => {
      mails.push(m);
    },
    checkout: async (s) => {
      sessions.push(s.accountId);
      return { id: `cs_test${sessions.length}`, url: `https://coinpayportal.com/checkout/cs_test${sessions.length}` };
    },
    now: () => clock,
    log: () => {},
  });
  return { g, store, w, mails, sessions, tick: (days: number) => (clock = new Date(clock.getTime() + days * 86_400_000)) };
}

const sub = (o: Partial<Submission> = {}): Submission => {
  const p = parseSubmission({ url: "https://example.com", name: "Example", contact_email: "me@example.com", ...o });
  if (typeof p === "string") throw new Error(p);
  return p;
};

describe("normalizeUrl", () => {
  test("keys by host without www and path without trailing slash", () => {
    expect(normalizeUrl("Example.com/")).toEqual({ url: "https://example.com", key: "example.com", host: "example.com" });
    expect(normalizeUrl("https://www.Example.com/music/?x=1#y")?.key).toBe("example.com/music");
    expect(normalizeUrl("http://www.example.com/a//b/")?.url).toBe("http://www.example.com/a/b");
  });
  test("refuses what we never list", () => {
    for (const bad of ["", "ftp://x.com", "https://user:pw@x.com", "https://x.com:8080", "http://127.0.0.1", "https://[::1]/", "localhost", "https://x".repeat(80)]) {
      expect(normalizeUrl(bad)).toBeNull();
    }
  });
});

describe("SSRF guard", () => {
  test("private and special addresses", () => {
    for (const ip of ["127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1", "224.0.0.1"]) {
      expect(isPrivateAddress(ip)).toBe(true);
    }
    for (const ip of ["93.184.216.34", "1.1.1.1", "2606:4700::1111"]) expect(isPrivateAddress(ip)).toBe(false);
  });
  test("refuses a host resolving private, and a redirect into one", async () => {
    const w = web({ "https://example.com": { status: 302, location: "http://internal.example/" } });
    await expect(safeFetch("https://evil.example", { fetch: w.fetch, resolve: async () => ["10.0.0.5"] })).rejects.toBeInstanceOf(FetchRefused);
    const resolve = async (h: string) => (h === "internal.example" ? ["127.0.0.1"] : ["93.184.216.34"]);
    await expect(safeFetch("https://example.com", { fetch: w.fetch, resolve })).rejects.toThrow(/private/);
    await expect(safeFetch("https://example.com:2375/", { fetch: w.fetch, resolve })).rejects.toThrow(/port/);
  });
});

describe("badge", () => {
  test("detects the snippet, a bare gallery link and an img, not lookalikes", () => {
    expect(hasBadge(BADGE_HTML)).toBe(true);
    expect(hasBadge(`<a class="x" href='https://c0mpute.com/gallery'>listed</a>`)).toBe(true);
    expect(hasBadge(`<img alt="" src="//c0mpute.com/badge.svg?url=x">`)).toBe(true);
    expect(hasBadge(`<a href="https://c0mpute.com/galleryx">`)).toBe(false);
    expect(hasBadge(`<a href="https://notc0mpute.com/gallery">`)).toBe(false);
    expect(hasBadge(`<p>c0mpute.com/gallery</p>`)).toBe(false);
  });
  test("keyed by ?url=, the Referer as fallback, never our own pages", () => {
    expect(badgeKey("https://www.example.com/", null)).toBe("example.com");
    expect(badgeKey(null, "https://example.com/blog/post?x")).toBe("example.com/blog/post");
    expect(badgeKey(null, "https://c0mpute.com/gallery/submit")).toBeNull();
    expect(badgeKey(null, null)).toBeNull();
  });
  test("snippet, svg and hit buffer", () => {
    expect(badgeSnippet("https://example.com/a b")).toBe(
      `<a href="https://c0mpute.com/gallery?ref=https%3A%2F%2Fexample.com%2Fa%20b"><img src="https://c0mpute.com/badge.svg?url=https%3A%2F%2Fexample.com%2Fa%20b" alt="Listed on the c0mpute gallery" width="170" height="36"></a>`,
    );
    expect(badgeSvg("dark")).toContain("#0a0a0b");
    expect(badgeSvg("light")).toContain('width="170"');
    countHit("example.com");
    countHit("example.com");
    expect(takeHits().get("example.com")).toBe(2);
    expect(takeHits().size).toBe(0);
  });
  test("checkBadge looks at the page, then the homepage", async () => {
    const w = web({ "https://example.com/app": { html: "<p>hi</p>" }, "https://example.com": { html: BADGE_HTML } });
    expect((await checkBadge("https://example.com/app", w)).state).toBe("found");
    const down = web({});
    expect((await checkBadge("https://example.com", down)).state).toBe("error");
  });
});

describe("store", () => {
  test("seeds the editorial list once, as approved, in feed order", async () => {
    const sql = memSql();
    const store = createGalleryStore(sql);
    const list = (await store.approved()).map(toEntry);
    expect(list.map((e) => e.slug)).toEqual(GALLERY.map((e) => e.slug));
    expect(list[0]).toMatchObject({ ...GALLERY[0], featured: false, listed_via: "editorial" });
    await store.remove("seed-tax-guy-agent");
    // The next process on the same database does not bring a deleted seed back.
    expect((await createGalleryStore(sql).approved()).length).toBe(GALLERY.length - 1);
  });
});

describe("feed", () => {
  test("schema_version 1, every old field kept, new ones added", () => {
    const r = galleryResponse(GALLERY);
    expect(r.schema_version).toBe(GALLERY_SCHEMA_VERSION);
    expect(r.schema_version).toBe(1);
    expect(r.submit_url).toBe("https://c0mpute.com/gallery/submit");
    expect(Object.keys(r.data[0])).toEqual(expect.arrayContaining(["slug", "name", "tagline", "url", "c0mpute_url", "category", "built_with", "image", "added_at", "featured", "listed_via"]));
    expect(r.data[0].image).toBe("https://c0mpute.com/gallery/board-saas.png");
  });
});

describe("submit", () => {
  test("validation", () => {
    expect(parseSubmission({})).toMatch(/^url/);
    expect(parseSubmission({ url: "x.com", tier: "gold" })).toMatch(/^tier/);
    expect(parseSubmission({ url: "x.com", category: "nope" })).toMatch(/^category/);
    expect(parseSubmission({ url: "x.com", tier: "priority" })).toMatch(/^contact_email/);
    expect(parseSubmission({ url: "x.com", email: "bad" })).toMatch(/^contact_email/);
    expect(parseSubmission({ url: "x.com", built_with: "c0mpute hosting, fine-tune,," })).toMatchObject({ name: "x.com", built_with: ["c0mpute hosting", "fine-tune"], tier: "free", category: "apps" });
  });

  test("free without the badge: queued 30 days, emailed with the snippet", async () => {
    const { g, mails } = setup({ "https://example.com": { html: "<p>hello</p>" } });
    const r = await g.submit(sub());
    expect(r.listing.status).toBe("queued");
    expect(r.listing.goes_live_at).toBe("2026-12-22T12:00:00.000Z");
    expect(r.listing.badge_state).toBe("missing");
    expect(mails[0].subject).toMatch(/queue/);
    expect(mails[0].text).toContain("badge.svg?url=https%3A%2F%2Fexample.com");
    expect(mails[0].html).toContain("<pre");
  });

  test("free with the badge: listed at once, in the feed", async () => {
    const { g, store, mails } = setup({ "https://example.com": { html: BADGE_HTML } });
    const r = await g.submit(sub());
    expect(r.listing).toMatchObject({ status: "approved", listed_via: "badge", added_at: "2026-11-22", tier: "free" });
    expect((await store.approved()).map((l) => l.url)).toContain("https://example.com");
    expect(mails.map((m) => m.subject)).toEqual(["You are listed on the c0mpute gallery: Example"]);
    expect(toEntry(r.listing).image).toBe(`/gallery/card/${r.listing.slug}`);
  });

  test("refused when the URL is not 200 or the domain is blocklisted; nothing stored", async () => {
    const { g, store } = setup({});
    await expect(g.submit(sub())).rejects.toMatchObject({ status: 422 });
    expect(await store.byKey("example.com")).toBeNull();
    const s2 = setup({ "https://example.com": { html: "ok" } });
    const bad = createGalleryService({ store: s2.store, fetchDeps: s2.w, reputation: async () => ({ listed: true, reasons: ["Spamhaus DBL: listed"] }), log: () => {} });
    await expect(bad.submit(sub())).rejects.toThrow(/blocklisted/);
  });

  test("adult label holds for a person", async () => {
    const { g } = setup({ "https://example.com": { html: `<meta name="rating" content="adult">${BADGE_HTML}` } });
    expect((await g.submit(sub())).listing.status).toBe("review");
  });

  test("dedupe by normalized URL", async () => {
    const { g } = setup({ "https://example.com": { html: "x" }, "https://www.example.com": { html: "x" } });
    await g.submit(sub());
    const e = await g.submit(sub({ url: "https://www.example.com/" })).catch((x) => x);
    expect(e).toBeInstanceOf(GalleryError);
    expect(e.status).toBe(409);
    expect(e.listing.url).toBe("https://example.com");
  });
});

describe("check my badge now", () => {
  test("approves a queued listing once the badge appears; cooldown", async () => {
    const pages: Record<string, Page> = { "https://example.com": { html: "<p>no badge</p>" } };
    const { g } = setup(pages);
    const { listing } = await g.submit(sub());
    const miss = await g.checkNow(listing, { manual: true });
    expect(miss.badge.state).toBe("missing");
    await expect(g.checkNow(miss.listing, { manual: true })).rejects.toMatchObject({ status: 429 });
    pages["https://example.com"] = { html: BADGE_HTML };
    const later = { ...miss.listing, manual_check_at: "2000-01-01T00:00:00Z" };
    const hit = await g.checkNow(later, { manual: true });
    expect(hit.listing.status).toBe("approved");
    expect(hit.message).toMatch(/listed now/);
  });
});

describe("priority", () => {
  test("checkout opened, paid webhook approves after checks, idempotent", async () => {
    const { g, sessions } = setup({ "https://example.com": { html: "<p>site</p>" } });
    const r = await g.submit(sub({ tier: "priority" }));
    expect(r.listing.status).toBe("awaiting_payment");
    expect(r.checkout_url).toBe("https://coinpayportal.com/checkout/cs_test1");
    expect(sessions).toEqual([r.listing.id]);
    const ev = { paid: true, checkoutSessionId: "cs_test1", accountId: r.listing.id, amountUsd: GALLERY_CONFIG.priorityPriceUsd, metadata: { kind: "gallery_priority", listing_id: r.listing.id } };
    expect((await g.onPaid({ ...ev, metadata: { kind: "other" } })).action).toMatch(/ignored/);
    const done = await g.onPaid(ev);
    expect(done.action).toBe("approved");
    expect(done.listing).toMatchObject({ status: "approved", listed_via: "priority", tier: "priority" });
    expect((await g.onPaid(ev)).action).toBe("already settled");
  });

  test("a priority submission whose site has the badge is listed free, no checkout", async () => {
    const { g, sessions } = setup({ "https://example.com": { html: BADGE_HTML } });
    const r = await g.submit(sub({ tier: "priority" }));
    expect(r.listing).toMatchObject({ status: "approved", tier: "free" });
    expect(sessions).toEqual([]);
  });

  test("admin reject flags a refund and emails", async () => {
    const { g, mails } = setup({ "https://example.com": { html: "<p>site</p>" } });
    const r = await g.submit(sub({ tier: "priority" }));
    const paid = await g.store.update(r.listing.id, { paid_at: "2026-11-22T12:01:00Z", status: "priority_review" });
    const rej = await g.admin("reject", paid!, { reason: "not built on c0mpute", refund: true });
    expect(rej).toMatchObject({ status: "rejected", refund_requested: true, reject_reason: "not built on c0mpute" });
    expect(mails.at(-1)!.text).toMatch(/refund/);
  });

  test("webhook signature: unsigned or forged is refused", () => {
    const body = JSON.stringify({ type: "payment.confirmed", data: { payment_id: "p1", status: "confirmed" } });
    expect(verifyWebhook(body, null, "whsec")).toBe(false);
    expect(verifyWebhook(body, "t=1,v1=deadbeef", "whsec")).toBe(false);
    expect(verifyWebhook(body, signWebhook(body, "other"), "whsec")).toBe(false);
    expect(verifyWebhook(body, signWebhook(body, "whsec"), "whsec")).toBe(true);
  });
});

describe("background pass", () => {
  test("queue promotion after 30 days", async () => {
    const { g, tick } = setup({ "https://example.com": { html: "<p>x</p>" } });
    const { listing } = await g.submit(sub());
    tick(29);
    expect((await g.tick()).promoted).toBe(0);
    tick(1.01);
    expect((await g.tick()).promoted).toBe(1);
    expect((await g.store.byId(listing.id))!).toMatchObject({ status: "approved", listed_via: "queue" });
  });

  test("badge gone on two checks a day apart: back to the queue with a notice", async () => {
    const pages: Record<string, Page> = { "https://example.com": { html: BADGE_HTML } };
    const { g, tick, mails } = setup(pages);
    const { listing } = await g.submit(sub());
    expect(listing.status).toBe("approved");
    pages["https://example.com"] = { html: "<p>badge removed</p>" };
    tick(8);
    expect((await g.tick()).requeued).toBe(0);
    expect((await g.store.byId(listing.id))!.badge_missing_since).not.toBeNull();
    tick(1.1);
    expect((await g.tick()).requeued).toBe(1);
    const back = (await g.store.byId(listing.id))!;
    expect(back).toMatchObject({ status: "queued", listed_via: null });
    expect(mails.at(-1)!.subject).toMatch(/badge is gone/);
    expect(publicStatus(back).goes_live_at).toBe(back.goes_live_at);
  });

  test("a site that is down is not a missing badge", async () => {
    const pages: Record<string, Page> = { "https://example.com": { html: BADGE_HTML } };
    const { g, tick } = setup(pages);
    const { listing } = await g.submit(sub());
    pages["https://example.com"] = { status: 503 };
    tick(8);
    await g.tick();
    tick(1.1);
    await g.tick();
    expect((await g.store.byId(listing.id))!.status).toBe("approved");
  });

  test("a queued listing that places the badge later is picked up by the daily scan", async () => {
    const pages: Record<string, Page> = { "https://example.com": { html: "<p>x</p>" } };
    const { g, tick } = setup(pages);
    const { listing } = await g.submit(sub());
    pages["https://example.com"] = { html: BADGE_HTML };
    tick(1.1);
    expect((await g.tick()).badgeApproved).toBe(1);
    expect((await g.store.byId(listing.id))!.listed_via).toBe("badge");
  });
});

describe("mail copy", () => {
  test("no em dashes", () => {
    const l = { id: "gl_x", slug: "x", url: "https://x.com", name: "X", status: "queued", goes_live_at: "2026-12-22T00:00:00Z", listed_via: "badge", paid_at: null, reject_reason: null } as never;
    for (const k of ["submitted", "approved", "badge_missing", "rejected"] as const) {
      const b = mailBody(k, l);
      expect([b.subject, ...b.paras].join(" ")).not.toContain("—");
    }
  });
});

describe("mcp gallery tools", () => {
  test("listed when wired; submit and status", async () => {
    const { g } = setup({ "https://example.com": { html: BADGE_HTML } });
    const upstream = (async () => new Response("{}")) as never;
    const user = { id: "u1", did: "did:key:x", name: null };
    const list = (await handleRpc({ jsonrpc: "2.0", id: 1, method: "tools/list" }, { user, upstream, gallery: g })) as { result: { tools: { name: string }[] } };
    expect(list.result.tools.map((t) => t.name)).toEqual(expect.arrayContaining(["gallery_submit", "gallery_status", "gallery_list"]));
    const call = async (name: string, args: Record<string, unknown>) =>
      JSON.parse(((await handleRpc({ jsonrpc: "2.0", id: 2, method: "tools/call", params: { name, arguments: args } }, { user, upstream, gallery: g })) as { result: { content: { text: string }[] } }).result.content[0].text);
    expect((await call("gallery_submit", { url: "example.com" })).listing.status).toBe("approved");
    expect((await call("gallery_status", { url: "https://www.example.com" })).listing.listed_via).toBe("badge");
    expect((await call("gallery_list", {})).data.map((e: { url: string }) => e.url)).toContain("https://example.com");
  });
});
