import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { createHmac } from "node:crypto";
import type { Sql } from "../chat/store";
import {
  MIN_PARTICIPANTS,
  PII_PATTERNS,
  cleanLabel,
  foldSmallLabels,
  handleFor,
  leaderboardSecret,
  personToken,
  piiFindings,
  roundCount,
  scrubPii,
  spendBucket,
} from "./anon";
import { WINDOW_KEYS, buildLeaderboard, type InfernetWindow, type LeaderboardInputs, type UsageRow, type Window } from "./build";
import { createLeaderboardStore, nodeWindowStats } from "./store";

function memSql(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}

// Built at runtime: fake values, but shaped like real ones on purpose (secret scanners flag literals).
const CLIENT_KEY = ["ifc", "test".repeat(8)].join("_");
const SECRET = leaderboardSecret({ INFERNET_CLIENT_KEY: CLIENT_KEY })!;
const SUBS = ["sub-alice", "sub-bob", "sub-carol", "sub-dave"];
const TOK = Object.fromEntries(SUBS.map((s) => [s, personToken(CLIENT_KEY, s)]));
const EMAIL = "alice@example.com";
const DID = ["did", "key", "z6Mk" + "Ab".repeat(20)].join(":");

const perWindow = <T,>(v: T) => Object.fromEntries(WINDOW_KEYS.map((w) => [w, v])) as Record<Window, T>;

function infernetWindow(): InfernetWindow {
  return {
    providers: [
      { c0mpute_node: "ffig7P4J", owner: null, provider: "p1".repeat(32), earned_usd: 0.5, jobs: 50 },
      { owner: TOK["sub-bob"], provider: "p2".repeat(32), earned_usd: 0.2, jobs: 20 },
      { owner: null, provider: "p3".repeat(32), earned_usd: 0.1, jobs: 10 },
    ],
    spenders: [
      { who: TOK["sub-alice"], spent_usd: 3.21, jobs: 321 },
      { who: TOK["sub-bob"], spent_usd: 0.05, jobs: 7 },
      { who: TOK["sub-carol"], spent_usd: 0.004, jobs: 2 },
    ],
    referrers: [
      { who: TOK["sub-alice"], commissions_usd: 0.003, nodes: 2 },
      { who: TOK["sub-bob"], commissions_usd: 0.001, nodes: 1 },
      { who: TOK["sub-dave"], commissions_usd: 0, nodes: 1 },
    ],
    models: [
      { model: "qwen2.5:7b", jobs: 900, output_tokens: 123456, users: 5 },
      { model: "alice-private-finetune", jobs: 40, output_tokens: 999, users: 1 },
    ],
    totals: { spenders: 3, spent_usd: 3.264, billed_jobs: 330, earning_providers: 3, served_jobs: 940, output_tokens: 124455 },
  };
}

function usage(): UsageRow[] {
  return [
    { token: TOK["sub-alice"], surface: "mcp", item: "chat", calls: 12 },
    { token: TOK["sub-bob"], surface: "mcp", item: "chat", calls: 3 },
    { token: TOK["sub-carol"], surface: "mcp", item: "chat", calls: 1 },
    { token: TOK["sub-alice"], surface: "mcp", item: "hosting_push", calls: 4 },
    { token: TOK["sub-alice"], surface: "api", item: "qwen2.5:7b", calls: 250 },
    { token: TOK["sub-bob"], surface: "api", item: "auto", calls: 30 },
    { token: TOK["sub-dave"], surface: "api", item: "auto", calls: 2 },
    { token: TOK["sub-carol"], surface: "chat", item: "auto", calls: 9 },
  ];
}

function inputs(over: Partial<LeaderboardInputs> = {}): LeaderboardInputs {
  return {
    generatedAt: "2026-11-22T12:00:00.000Z",
    secret: SECRET,
    workersOnline: 17,
    nodes: [
      { id: "ffig7P4J", class: "cpu", bench_score: 900, name: "profullstack", last_seen_s: 5 },
      { id: "AmaKBdpB", class: "cpu", bench_score: 1200, name: null, last_seen_s: 5 },
      { id: "Xyz12345", class: "gpu", bench_score: null, name: "solo", last_seen_s: 5 },
    ],
    referralHandles: [{ ref: "profullstack", nodes: 6 }],
    nodeStats: perWindow([
      { node: "ffig7P4J", jobs: 100, busy_s: 7200, native_jobs: 40, native_paid_usd: 1.0, active_days: 3 },
      { node: "AmaKBdpB", jobs: 60, busy_s: 3600, native_jobs: 60, native_paid_usd: 0.4, active_days: 2 },
      { node: "Gone0001", jobs: 5, busy_s: 60, native_jobs: 5, native_paid_usd: 0.02, active_days: 1 },
    ]),
    infernet: perWindow(infernetWindow()),
    usage: perWindow(usage()),
    paidToOperators: perWindow(2.12),
    hiddenTokens: new Set(),
    hiddenNodes: new Set(),
    history: { node_jobs_from: "2026-11-18", usage_from: null },
    ...over,
  };
}

describe("pseudonyms", () => {
  test("stable for one person, distinct between people", () => {
    expect(handleFor(SECRET, TOK["sub-alice"])).toBe(handleFor(SECRET, personToken(CLIENT_KEY, "sub-alice")));
    expect(handleFor(SECRET, TOK["sub-alice"])).not.toBe(handleFor(SECRET, TOK["sub-bob"]));
    expect(handleFor(SECRET, TOK["sub-alice"])).toMatch(/^anon-[0-9a-f]{6}$/);
  });

  test("not reversible: a plain hash of the sub, email or DID never matches", () => {
    const h = handleFor(SECRET, TOK["sub-alice"]).slice(5);
    const sha = (s: string) => createHmac("sha256", "").update(s).digest("hex").slice(0, 6);
    const plain = (s: string) => new Bun.CryptoHasher("sha256").update(s).digest("hex").slice(0, 6);
    for (const guess of ["sub-alice", EMAIL, DID]) {
      expect(h).not.toBe(plain(guess));
      expect(h).not.toBe(sha(guess));
    }
    // A different secret gives a different handle: without ours, nobody can link one.
    const other = leaderboardSecret({ LEADERBOARD_SECRET: "some-other-secret-0123456789" })!;
    expect(handleFor(other, TOK["sub-alice"])).not.toBe(handleFor(SECRET, TOK["sub-alice"]));
  });

  test("no secret configured: no secret, so pseudonymous boards are withheld", () => {
    expect(leaderboardSecret({})).toBeNull();
    expect(leaderboardSecret({ INFERNET_CLIENT_KEY: "short" })).toBeNull();
    const p = buildLeaderboard(inputs({ secret: null }));
    expect(p.boards["7d"].users.rows).toEqual([]);
    expect(p.boards["7d"].users.withheld).toContain("secret");
    expect(p.boards["7d"].affiliates.rows).toEqual([]);
  });
});

describe("helpers", () => {
  test("spend buckets and rounded counts", () => {
    expect(spendBucket(0)).toBe("$0");
    expect(spendBucket(0.004)).toBe("< $0.01");
    expect(spendBucket(3.21)).toBe("$1 to $10");
    expect(spendBucket(5000)).toBe("$1k+");
    expect(roundCount(7)).toBe(7);
    expect(roundCount(321)).toBe(320);
    expect(roundCount(12_345)).toBe(12_300);
  });

  test("labels: unknown shapes become other", () => {
    expect(cleanLabel("qwen2.5:7b")).toBe("qwen2.5:7b");
    expect(cleanLabel("eriktonon/qwen3.8-uncensored:9b")).toBe("eriktonon/qwen3.8-uncensored:9b");
    expect(cleanLabel("tell me a secret")).toBe("other");
    expect(cleanLabel(42)).toBe("other");
  });

  test("labels fewer than MIN_PARTICIPANTS people used fold into other", () => {
    const f = foldSmallLabels([
      { label: "a", calls: 10, users: MIN_PARTICIPANTS },
      { label: "b", calls: 99, users: 1 },
    ]);
    expect(f.map((r) => r.label)).toEqual(["a", "other"]);
    expect(f[1].calls).toBe(99);
  });

  test("PII patterns catch the obvious and scrubPii hides them", () => {
    for (const bad of [EMAIL, DID, "0x" + "52908400Ab".repeat(4), "bc1q" + "ar0s".repeat(9), "c0k" + "_abc123", "ifc" + "_deadbeef", "12D3KooW" + "AbCdEf".repeat(6), ["10", "0", "0", "1"].join("."), ["aaaabbbb", "1234", "4abc", "8def", "0123456789ab"].join("-")]) {
      expect(PII_PATTERNS.some((re) => re.test(bad))).toBe(true);
    }
    for (const ok of ["profullstack", "ffig7P4J", "anon-7f3a2c", "qwen2.5:7b", "2026-11-22T12:00:00.000Z", "$1 to $10"]) {
      expect(PII_PATTERNS.some((re) => re.test(ok))).toBe(false);
    }
    const s = scrubPii({ a: [EMAIL, "fine"] });
    expect(s.value).toEqual({ a: ["[hidden]", "fine"] });
    expect(s.scrubbed).toBe(1);
  });
});

describe("buildLeaderboard", () => {
  const p = buildLeaderboard(inputs());
  const json = JSON.stringify(p);

  test("no PII fields or values anywhere in the payload", () => {
    expect(piiFindings(p)).toEqual([]);
    for (const raw of [...SUBS, ...Object.values(TOK), EMAIL, DID, CLIENT_KEY, "p1p1p1", "p2p2p2", "alice-private-finetune"]) {
      expect(json).not.toContain(raw);
    }
    expect(json).not.toMatch(/"(email|did|sub|coinpay_sub|user_id|wallet|address|ip|token|owner|who|provider|prompt)"/);
  });

  test("nodes: ranked by jobs, public ids and nicknames only", () => {
    const rows = p.boards["7d"].nodes.rows;
    expect(rows.map((r) => r.node)).toEqual(["ffig7P4J", "AmaKBdpB", "Gone0001"]);
    expect(rows[0].operator).toBe("profullstack");
    expect(rows[0].busy_hours).toBe(2);
    expect(rows[2].online).toBe(false);
    // A node with a bench score but no jobs is still listed, after the ones that served.
    expect(p.boards["7d"].nodes.participants).toBe(3);
  });

  test("earners: nickname, else node id, else anon; c0mpute pay after the fee plus Infernet once", () => {
    const rows = p.boards["7d"].earners.rows;
    const byOp = Object.fromEntries(rows.map((r) => [r.operator, r]));
    // profullstack: native $1.00 * 0.95 + Infernet $0.50 for the same node.
    expect(byOp.profullstack.earned_usd).toBeCloseTo(1.45, 6);
    expect(byOp.profullstack.jobs).toBe(90);
    expect(byOp["node AmaKBdpB"].earned_usd).toBeCloseTo(0.38, 6);
    expect(byOp["node AmaKBdpB"].kind).toBe("node");
    const anon = rows.filter((r) => r.kind === "anon");
    expect(anon.length).toBe(2);
    expect(anon[0].operator).toBe(handleFor(SECRET, TOK["sub-bob"]));
    expect(rows[0].operator).toBe("profullstack");
  });

  test("users: spend shown as a range, counts rounded, ranked by spend", () => {
    const rows = p.boards["7d"].users.rows;
    expect(rows[0]).toMatchObject({ user: handleFor(SECRET, TOK["sub-alice"]), spend: "$1 to $10", billed_jobs: 320 });
    expect(rows[0].calls).toBe(270); // 266 calls, rounded
    expect(rows.every((r) => !/\d\.\d{3}/.test(r.spend))).toBe(true);
    expect(p.headline.active_users["7d"]).toBe(4);
  });

  test("API and agents: small models and tools fold into other; callers are handles", () => {
    const models = p.boards["7d"].api.models.rows;
    expect(models.map((r) => r.label)).toEqual(["qwen2.5:7b", "other"]);
    expect(models[1].users).toBe(`${MIN_PARTICIPANTS}+`);
    const tools = p.boards["7d"].agents.tools.rows;
    expect(tools.map((r) => r.label)).toEqual(["chat", "other"]); // hosting_push: one agent only
    expect(p.boards["7d"].api.callers.rows[0].user).toBe(handleFor(SECRET, TOK["sub-alice"]));
    expect(p.boards["7d"].api.totals).toEqual({ api_calls: 282, chat_calls: 9, served_jobs: 940, output_tokens: 124455 });
    expect(p.boards["7d"].agents.totals.mcp_calls).toBe(20);
  });

  test("small groups are withheld", () => {
    const two = usage().filter((u) => u.token !== TOK["sub-carol"] && u.surface === "mcp");
    const q = buildLeaderboard(inputs({ usage: perWindow(two) }));
    expect(q.boards["7d"].agents.agents.rows).toEqual([]);
    expect(q.boards["7d"].agents.agents.withheld).toMatch(/fewer than 3/);
    expect(q.boards["7d"].agents.tools.rows).toEqual([]);
    const lonely = buildLeaderboard(inputs({ infernet: null, usage: perWindow([usage()[0]]) }));
    expect(lonely.boards["7d"].users.rows).toEqual([]);
    expect(lonely.boards["7d"].users.withheld).toBeDefined();
    expect(lonely.boards["7d"].affiliates.rows).toEqual([]);
  });

  test("opt-out hides the person everywhere and their key-claimed nodes", () => {
    const q = buildLeaderboard(inputs({ hiddenTokens: new Set([TOK["sub-alice"]]), hiddenNodes: new Set(["ffig7P4J"]) }));
    const alice = handleFor(SECRET, TOK["sub-alice"]);
    expect(JSON.stringify(q)).not.toContain(alice);
    expect(q.boards["7d"].nodes.rows.map((r) => r.node)).not.toContain("ffig7P4J");
    expect(q.boards["7d"].earners.rows.map((r) => r.operator)).not.toContain("profullstack");
  });

  test("jobs served counts each job once across the two ledgers", () => {
    // native c0mpute jobs 40 + 60 + 5, plus Infernet-settled 940.
    expect(p.headline.jobs_served["7d"]).toBe(1045);
    expect(p.headline.paid_to_operators_usd?.["7d"]).toBe(2.12);
  });

  test("referral handles from the public board", () => {
    expect(p.referral_handles).toEqual([{ rank: 1, handle: "profullstack", nodes: 6 }]);
  });
});

describe("store", () => {
  test("usage counters, joined to subs, and opt-out", async () => {
    const sql = memSql();
    await sql.run(`CREATE TABLE chat_users (id TEXT PRIMARY KEY, coinpay_sub TEXT, did TEXT, name TEXT, created_at TEXT, last_login_at TEXT)`);
    await sql.run(`INSERT INTO chat_users VALUES ('u1', 'sub-alice', 'did:x:1', null, '', ''), ('u2', 'sub-bob', 'did:x:2', null, '', '')`);
    const s = createLeaderboardStore(sql);
    const day = Date.parse("2026-11-22T10:00:00Z");
    await s.record("u1", "mcp", "chat", day);
    await s.record("u1", "mcp", "chat", day);
    await s.record("u1", "api", "a prompt that is not a model id", day);
    await s.record("u2", "chat", "auto", day - 10 * 86_400_000);
    const recent = await s.usageSince(day - 86_400_000);
    expect(recent).toContainEqual({ sub: "sub-alice", surface: "mcp", item: "chat", calls: 2 });
    expect(recent).toContainEqual({ sub: "sub-alice", surface: "api", item: "other", calls: 1 });
    expect(recent.some((r) => r.sub === "sub-bob")).toBe(false);
    expect((await s.usageSince(0)).some((r) => r.sub === "sub-bob")).toBe(true);
    expect(await s.firstDay()).toBe("2026-11-12");

    await sql.run(`CREATE TABLE operator_nodes (user_id TEXT, node TEXT, via TEXT)`);
    await sql.run(`INSERT INTO operator_nodes VALUES ('u1', 'KeyNode1', 'key'), ('u1', 'WebNode1', 'web')`);
    expect(await s.isOptedOut("u1")).toBe(false);
    await s.setOptOut("u1", true);
    expect(await s.isOptedOut("u1")).toBe(true);
    expect(await s.hidden()).toEqual({ subs: ["sub-alice"], nodes: ["KeyNode1"] });
    await s.setOptOut("u1", false);
    expect(await s.hidden()).toEqual({ subs: [], nodes: [] });
  });

  test("node window stats split Infernet-settled jobs out of native pay", async () => {
    const sql = memSql();
    await sql.run(`CREATE TABLE node_jobs (job_id TEXT PRIMARY KEY, node TEXT, type TEXT, completed_at INTEGER, busy_s REAL, paid_usd REAL)`);
    const t = Date.parse("2026-11-22T10:00:00Z");
    await sql.run(
      `INSERT INTO node_jobs VALUES ('j1','N1','tts.synthesize',?,10,0.01), ('j2','N1','infernet.inference',?,20,0.5), ('j3','N1','tts.synthesize',?,5,0.02)`,
      [t, t, t - 86_400_000],
    );
    const [r] = await nodeWindowStats(sql, 0, ["infernet.inference"]);
    expect(r).toEqual({ node: "N1", jobs: 3, busy_s: 35, native_jobs: 2, native_paid_usd: 0.03, active_days: 2 });
  });
});
