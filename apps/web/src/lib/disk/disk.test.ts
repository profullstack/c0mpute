import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import type { Sql } from "../chat/store";
import { cleanDue, createDiskStore, parseSiteClaim, type GatewayDue } from "./store";
import { hoursToRecord, LOOKBACK_S, recordFrom, SETTLE_LAG_S } from "./recorder";
import { buildDiskReport } from "./report";
import { cleanReport, createAppUsageStore, priceParts, type UsageReport } from "./apps";
import { recordAppsFrom } from "./recorder";

function memSql(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}

const MINE = "12D3KooWSFxwveBjrGy4n1Djg3vpQD5aHPCXYCQHZWruffig7P4J"; // board id ffig7P4J
const THEIRS = "12D3KooWAbCdEfGhJkLmNpQrStUvWxYz123456789abcdeAmaKBdpB"; // AmaKBdpB
const OWNER_ME = "a".repeat(64);
const OWNER_THEM = "b".repeat(64);
const H = 3_600_000;
const GIB = 2 ** 30;

const due = (o: Partial<GatewayDue>): GatewayDue => ({
  site_id: "0123456789abcdef0123",
  name: "site",
  owner: OWNER_THEM,
  payer: OWNER_THEM,
  holder: MINE,
  bytes: GIB,
  price_per_gib_month_micros: 720_000, // $0.72/GiB-month = $0.001/GiB-hour
  amount_micros: 1_000,
  passed: 6,
  challenged: 6,
  payout: true,
  ...o,
});

describe("parseSiteClaim", () => {
  test("site id, site link or owner key", () => {
    expect(parseSiteClaim(" DA5B7396B87DC287AE09 ")).toEqual({ kind: "site", value: "da5b7396b87dc287ae09" });
    expect(parseSiteClaim("https://gateway.c0mpute.com/s/da5b7396b87dc287ae09/index.html")).toEqual({ kind: "site", value: "da5b7396b87dc287ae09" });
    expect(parseSiteClaim(OWNER_ME)).toEqual({ kind: "owner", value: OWNER_ME });
    expect(parseSiteClaim("nope")).toBeNull();
    expect(parseSiteClaim(7)).toBeNull();
  });
});

describe("cleanDue", () => {
  test("keeps sane rows, drops junk, never passes more than challenged", () => {
    expect(cleanDue(due({}))).toMatchObject({ holder: MINE, amount_micros: 1_000 });
    expect(cleanDue({ ...due({}), holder: "<script>" })).toBeNull();
    expect(cleanDue({ ...due({}), amount_micros: -1 })).toBeNull();
    expect(cleanDue({ ...due({}), passed: 9, challenged: 3 })?.passed).toBe(3);
    expect(cleanDue({ ...due({}), owner: "zz" })?.owner).toBe("");
  });
});

describe("hoursToRecord", () => {
  const base = 1_800_000_000 - (1_800_000_000 % 3600);
  test("finished hours only, after the gossip lag, no further back than receipts last", () => {
    const now = base + SETTLE_LAG_S + 10;
    const hs = hoursToRecord(null, now);
    expect(hs.at(-1)).toEqual([base - 3600, base]);
    expect(hs.length).toBe(LOOKBACK_S / 3600);
    expect(hoursToRecord(base, now)).toEqual([]);
    expect(hoursToRecord(base - 3600, now)).toEqual([[base - 3600, base]]);
    // Just past the hour, before the lag: that hour waits.
    expect(hoursToRecord(base - 3600, base + 60)).toEqual([]);
    // A cursor from long ago is clamped to the lookback.
    expect(hoursToRecord(base - 100 * 3600, now).length).toBe(LOOKBACK_S / 3600);
  });
});

describe("recorder + store", () => {
  test("files each hour once; a better answer (more challenges) replaces it", async () => {
    const store = createDiskStore(memSql());
    const base = 1_800_000_000 - (1_800_000_000 % 3600);
    const now = base + SETTLE_LAG_S + 1;
    let challenged = 3;
    const asked: string[] = [];
    const fetcher = (async (url: string) => {
      asked.push(url);
      return new Response(JSON.stringify({ dues: [due({ challenged, passed: challenged }), { junk: true }] }));
    }) as unknown as typeof fetch;
    expect(await recordFrom("https://g", store, fetcher, now)).toBe(2);
    expect(asked[0]).toContain(`from=${base - 2 * 3600}&to=${base - 3600}`);
    expect(await recordFrom("https://g", store, fetcher, now)).toBe(0); // cursor holds
    challenged = 6;
    await store.recordHour((base - 3600) * 1000, [due({ challenged: 6, passed: 6, amount_micros: 2_000 })]);
    await store.recordHour((base - 3600) * 1000, [due({ challenged: 2, passed: 2, amount_micros: 1 })]);
    const rows = await store.dues({ sinceMs: 0, nodes: ["ffig7P4J"], sites: [], owners: [] });
    expect(rows.length).toBe(2);
    expect(rows.find((r) => r.hour === (base - 3600) * 1000)?.amount_micros).toBe(2_000);
    expect(await store.dues({ sinceMs: 0, nodes: [], sites: [], owners: [] })).toEqual([]);
    expect((await store.dues({ sinceMs: 0, nodes: [], sites: [], owners: [OWNER_THEM] })).length).toBe(2);
  });

  test("a gateway without the endpoint is skipped and the cursor stays", async () => {
    const store = createDiskStore(memSql());
    const fetcher = (async () => new Response("no", { status: 404 })) as unknown as typeof fetch;
    expect(await recordFrom("https://old", store, fetcher, 1_800_000_000)).toBe(0);
    expect(await store.cursor("https://old")).toBeNull();
  });

  test("site claims", async () => {
    const store = createDiskStore(memSql());
    await store.claim("u1", { kind: "site", value: "0123456789abcdef0123", label: "x" });
    await store.claim("u1", { kind: "site", value: "0123456789abcdef0123" });
    expect((await store.claims("u1")).length).toBe(1);
    expect(await store.unclaim("u1", "site", "0123456789abcdef0123")).toBe(true);
    expect(await store.unclaim("u1", "site", "0123456789abcdef0123")).toBe(false);
  });
});

describe("buildDiskReport", () => {
  const now = 1_800_000_000_000 - (1_800_000_000_000 % H) + 30 * 60_000;
  const h0 = now - 30 * 60_000 - H; // last full hour
  const row = (o: Partial<GatewayDue> & { hour?: number }) => {
    const d = due(o);
    return { ...d, node: d.holder.slice(-8), hour: o.hour ?? h0 };
  };

  test("earned on others' sites, spend on mine held elsewhere, own when both", () => {
    const mySite = "ffffffffffffffffffff";
    const r = buildDiskReport({
      range: "24h",
      now,
      rows: [
        row({ amount_micros: 4_000 }), // their site on my node: earned
        row({ site_id: mySite, owner: OWNER_ME, holder: THEIRS, amount_micros: 3_000 }), // my site elsewhere: spend
        row({ site_id: mySite, owner: OWNER_ME, holder: MINE, amount_micros: 2_000 }), // my site on my node: own
        row({ site_id: "eeeeeeeeeeeeeeeeeeee", holder: THEIRS, amount_micros: 9_999 }), // not mine at all
      ],
      nodes: [{ node: "ffig7P4J", label: "box", online: true, offer: { free_bytes: 5 * GIB, total_bytes: 10 * GIB, per_gib_month: 0.004 } }],
      sites: [{ site_id: mySite, via: "push", name: "photos", status: "live", url: null }],
      owners: [],
      live: [{ site_id: mySite, name: "photos", owner: OWNER_ME, size: 2 * GIB, copies: 3, price_per_gib_month: "0.72", preferred_hosts: [] }],
    });
    expect(r.totals.earned_usd).toBeCloseTo(0.004, 9);
    expect(r.totals.spend_usd).toBeCloseTo(0.003, 9);
    expect(r.totals.own_usd).toBeCloseTo(0.002, 9);
    expect(r.totals.net_usd).toBeCloseTo(0.001, 9);
    expect(r.buckets.reduce((a, b) => a + b.earned_usd, 0)).toBeCloseTo(0.004, 9);
    const site = r.sites[0];
    expect(site).toMatchObject({ name: "photos", live: true, size_bytes: 2 * GIB, copies: 3, price_per_gib_month: 0.72, holders: 2 });
    expect(site.url).toBe("https://gateway.c0mpute.com/s/ffffffffffffffffffff/");
    expect(site.monthly_usd).toBeCloseTo(1.44, 6); // 2 holders x 1 GiB x $0.72
    expect(r.nodes[0]).toMatchObject({ node: "ffig7P4J", held_bytes: 2 * GIB, sites: 2, passed: 12, challenged: 12 });
    expect(r.nodes[0].earned_usd).toBeCloseTo(0.004, 9);
  });

  test("an owner key claims every site of that key, live or in the ledger", () => {
    const r = buildDiskReport({
      range: "24h",
      now,
      rows: [row({ site_id: "11111111111111111111", owner: OWNER_ME, holder: THEIRS, amount_micros: 500 })],
      nodes: [],
      sites: [],
      owners: [OWNER_ME],
      live: [{ site_id: "22222222222222222222", name: "blog", owner: OWNER_ME, size: 10, copies: 3, price_per_gib_month: null, preferred_hosts: [] }],
    });
    expect(r.sites.map((s) => s.site_id).sort()).toEqual(["11111111111111111111", "22222222222222222222"]);
    expect(r.totals.spend_usd).toBeCloseTo(0.0005, 9);
    const blog = r.sites.find((s) => s.name === "blog")!;
    expect(blog).toMatchObject({ price_per_gib_month: 0.004, price_floor: true });
    expect(blog.monthly_usd).toBeCloseTo(((10 * 3) / 2 ** 30) * 0.004, 12); // no dues yet: size x copies x price
  });

  test("an hour spreads over 5-minute bars on the 1h range", () => {
    const r = buildDiskReport({ range: "1h", now, rows: [row({ amount_micros: 12_000 })], nodes: [{ node: "ffig7P4J", label: null, online: true }], sites: [], owners: [], live: [] });
    // Only the part of the hour inside the window counts: 30 of 60 minutes.
    expect(r.totals.earned_usd).toBeCloseTo(0.006, 9);
    expect(r.buckets.filter((b) => b.earned_usd > 0).length).toBe(6);
  });
});

describe("apps usage", () => {
  const H = 3_600_000;
  const now = 1_800_000_000_000 - (1_800_000_000_000 % H) + 30 * 60_000;
  const h0 = now - 30 * 60_000 - H;
  const rates = { cpu_hour: 10_000, ram_gib_hour: 2_000, disk_gib_month: 4_000, egress_gib: 10_000 };
  const report = (o: Partial<UsageReport> = {}): UsageReport => ({
    app_id: "aaaaaaaaaaaaaaaaaaaa",
    owner: OWNER_ME,
    holder: THEIRS,
    hour: h0 / 1000,
    secs: 3600,
    cpu_s: 1800,
    ram_gib_h: 1,
    disk_gib_h: 2,
    rx_bytes: 5,
    tx_bytes: 2 ** 30,
    rates,
    ...o,
  });

  test("cleanReport + priceParts match the node's rule", () => {
    const r = cleanReport(report())!;
    expect(priceParts(r).total).toBe(Math.floor(5_000 + 2_000 + (4_000 * 2) / 720 + 10_000));
    expect(cleanReport({ ...report(), hour: 7 })).toBeNull();
    expect(cleanReport({ ...report(), cpu_s: -1 })).toBeNull();
    expect(cleanReport({ ...report(), holder: "x" })).toBeNull();
  });

  test("recorder files app usage, re-reads two hours, stores priced rows", async () => {
    const sql = memSql();
    const apps = createAppUsageStore(sql);
    const cursors = createDiskStore(sql);
    const nowS = 1_800_000_000;
    const urls: string[] = [];
    const fetcher = (async (u: string) => {
      urls.push(u);
      return new Response(JSON.stringify({ usage: [report({ hour: Math.floor(nowS / 3600) * 3600 - 7200 })] }));
    }) as unknown as typeof fetch;
    expect(await recordAppsFrom("https://g", apps, cursors, fetcher, nowS)).toBe(1);
    expect(await recordAppsFrom("https://g", apps, cursors, fetcher, nowS)).toBe(1); // re-read, same row
    const rows = await apps.usage({ sinceMs: 0, nodes: [], apps: [], owners: [OWNER_ME] });
    expect(rows.length).toBe(1);
    expect(rows[0].amount_micros).toBeGreaterThan(17_000);
    expect(urls.length).toBe(2);
  });

  test("report: my app elsewhere is spend, split by part; their app on my node is earned", () => {
    const row = (o: Partial<UsageReport>) => {
      const r = report(o);
      return { ...r, node: r.holder.slice(-8), hour: r.hour * 1000, amount_micros: 0 };
    };
    const r = buildDiskReport({
      range: "24h",
      now,
      rows: [],
      nodes: [{ node: "ffig7P4J", label: null, online: true }],
      sites: [],
      owners: [OWNER_ME],
      live: [],
      appRows: [row({}), row({ app_id: "bbbbbbbbbbbbbbbbbbbb", owner: OWNER_THEM, holder: MINE, tx_bytes: 0, disk_gib_h: 0, ram_gib_h: 0 })],
      liveApps: [{ app_id: "aaaaaaaaaaaaaaaaaaaa", name: "api", owner: OWNER_ME, healthy: true, holders: [THEIRS], primary: THEIRS, replicas: 3 }],
      appIds: [],
    });
    expect(r.apps.length).toBe(1);
    const a = r.apps[0];
    expect(a).toMatchObject({ name: "api", healthy: true, via: "owner" });
    expect(a.cpu_hours).toBeCloseTo(0.5, 9);
    expect(a.cpu_usd).toBeCloseTo(0.005, 9);
    expect(a.egress_usd).toBeCloseTo(0.01, 9);
    expect(a.spend_usd).toBeCloseTo(0.017011, 5);
    expect(r.by_kind.apps.spend_usd).toBeCloseTo(0.017011, 5);
    expect(r.by_kind.apps.earned_usd).toBeCloseTo(0.005, 9);
    expect(r.totals.earned_usd).toBeCloseTo(0.005, 9);
    expect(r.nodes[0]).toMatchObject({ apps: 1 });
    expect(r.nodes[0].app_cpu_hours).toBeCloseTo(0.5, 9);
  });
});
