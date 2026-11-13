import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { createHash, randomBytes } from "node:crypto";
import { claimOnce, createStore, type Sql } from "./store";
import { createOAuthStore, OAuthError } from "./oauth";
import { createPluginStore } from "./plugin-store";
import { checkPluginUrl, isBlockedAddress, isPublicHostname, PluginUrlError } from "./plugin-net";
import { consumeTicket, makeTicket, ticketForOtherAccount, verifyTicket } from "./plugins";
import { createMediaStore } from "./media-owners";
import { gateGeneration } from "./adult";
import { musicPolicyRequest, parseMusicRequest } from "./music";
import { parseAlbumRequest } from "./album";
import { parsePublicPromoRequest } from "./promo-guard";
import { handleRpc } from "./mcp";
import { serverIsNewer } from "./sync-merge";
import { headerTimeout, ipBucket, rateLimit, rateWindows, RATE_WINDOWS_MAX, safeToRetry, sameOrigin, scopedUser } from "./server";
import type { Upstream } from "./completions";

function memSql(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}
const DID = "did:key:test-user-one";
const JOB = "0b6f1a52-7c1e-4d0a-9b53-3f2a1c9e8d77";

describe("OAuth-issued keys carry the mcp scope only", () => {
  test("an oauth key is a user on /api/mcp and signed out everywhere else", async () => {
    const s = createStore(memSql());
    const u = await s.upsertCoinpayUser({ sub: "x", did: DID });
    const oauth = await s.createOAuthKey(u.id, "Claude (OAuth)");
    const key = await s.createKey(u.id, "laptop");
    const viaOAuth = await s.userForToken(oauth);
    expect(viaOAuth?.token_kind).toBe("oauth");
    // What currentUser does with it: only the MCP route passes { oauth: true }.
    expect(scopedUser(viaOAuth, { oauth: true })?.id).toBe(u.id);
    expect(scopedUser(viaOAuth)).toBeNull(); // keys, cli/approve, me/adult, the REST API
    // An ordinary key and a session are untouched.
    expect(scopedUser(await s.userForToken(key))?.token_kind).toBe("key");
    expect(scopedUser(await s.userForToken(await s.createSession(u.id)))?.token_kind).toBe("session");
  });

  test("the person still sees and revokes an oauth key with their other keys", async () => {
    const s = createStore(memSql());
    const u = await s.upsertCoinpayUser({ sub: "x", did: DID });
    const oauth = await s.createOAuthKey(u.id, "ChatGPT (OAuth)");
    const listed = await s.listKeys(u.id);
    expect(listed.map((k) => k.label)).toEqual(["ChatGPT (OAuth)"]);
    expect(await s.revokeKeyById(u.id, listed[0]!.id)).toBe(true);
    expect(await s.userForToken(oauth)).toBeNull();
  });
});

describe("one-shot secrets are consumed atomically", () => {
  test("claimOnce is true for exactly one caller", async () => {
    const sql = memSql();
    await createStore(sql).init();
    const got = await Promise.all([claimOnce(sql, "a"), claimOnce(sql, "a"), claimOnce(sql, "a")]);
    expect(got.filter(Boolean)).toHaveLength(1);
    expect(await claimOnce(sql, "b")).toBe(true);
  });

  test("two exchanges racing one authorization code: one key", async () => {
    const o = createOAuthStore(memSql());
    const verifier = "a".repeat(43);
    const challenge = createHash("sha256").update(verifier).digest("base64url");
    const client = await o.registerClient({ redirect_uris: ["https://claude.ai/cb"], client_name: "Claude" });
    const code = await o.createCode({ client_id: client.client_id, user_id: "u1", redirect_uri: "https://claude.ai/cb", challenge });
    const p = { client_id: client.client_id, redirect_uri: "https://claude.ai/cb", code_verifier: verifier };
    const both = await Promise.allSettled([o.redeemCode(code, p), o.redeemCode(code, p)]);
    expect(both.filter((r) => r.status === "fulfilled")).toHaveLength(1);
    const lost = both.find((r) => r.status === "rejected") as PromiseRejectedResult;
    expect(lost.reason).toBeInstanceOf(OAuthError);
    await expect(o.redeemCode(code, p)).rejects.toThrow(/unknown or used/);
  });

  test("two polls racing an approved device code: one key; two approvers: one wins", async () => {
    const s = createStore(memSql());
    const a = await s.upsertCoinpayUser({ sub: "a", did: DID });
    const b = await s.upsertCoinpayUser({ sub: "b", did: DID });
    const d = await s.startDevice("laptop");
    const approvals = await Promise.all([s.approveDevice(d.user_code, a.id), s.approveDevice(d.user_code, b.id)]);
    expect(approvals.filter(Boolean)).toHaveLength(1);
    const polls = await Promise.all([s.pollDevice(d.device_code), s.pollDevice(d.device_code)]);
    const keys = polls.filter((p) => p.status === "ok") as { status: "ok"; key: string }[];
    expect(keys).toHaveLength(1);
    const winner = approvals[0] ? a.id : b.id;
    expect((await s.userForToken(keys[0]!.key))?.id).toBe(winner);
    expect((await s.pollDevice(d.device_code)).status).toBe("expired");
  });

  test("a plugin sign-in state is taken once, even by two callbacks at once", async () => {
    const s = createPluginStore(memSql(), { key: randomBytes(32) });
    await s.savePending("u1", "tc", "state-1", { verifier: "v" });
    const both = await Promise.all([s.takePending("state-1"), s.takePending("state-1")]);
    expect(both.filter(Boolean)).toHaveLength(1);
  });
});

describe("plugin connect tickets", () => {
  test("a ticket is used up by the confirming POST", async () => {
    process.env.C0MPUTE_PLUGIN_KEY ||= randomBytes(32).toString("hex");
    const s = createPluginStore(memSql(), { key: randomBytes(32) });
    const t = makeTicket("u1", "notes");
    // Looking at the confirm page does not use it up.
    expect(verifyTicket(t, "notes")).toBe("u1");
    expect(verifyTicket(t, "notes")).toBe("u1");
    expect(await consumeTicket(t, "notes", s)).toBe("u1");
    expect(await consumeTicket(t, "notes", s)).toBeNull();
    // A ticket for another plugin, a forged one and an expired one never work.
    expect(await consumeTicket(makeTicket("u1", "other"), "notes", s)).toBeNull();
    expect(await consumeTicket(`${t}x`, "notes", s)).toBeNull();
    expect(await consumeTicket(makeTicket("u1", "notes", Date.now() - 3_600_000), "notes", s)).toBeNull();
  });

  test("a link made by one account is refused in a browser signed in as another", () => {
    expect(ticketForOtherAccount("u_attacker", "u_victim")).toBe(true);
    expect(ticketForOtherAccount("u1", "u1")).toBe(false);
    // No session here (the CLI's link opened in a fresh browser): the confirm page decides.
    expect(ticketForOtherAccount("u1", null)).toBe(false);
  });
});

describe("media results belong to the account that started the job", () => {
  test("the owner reads it, nobody else does, and a job never changes hands", async () => {
    const m = createMediaStore(memSql());
    await m.record("u1", "image", JOB);
    expect(await m.allowed("u1", JOB)).toBe(true);
    expect(await m.allowed("u2", JOB)).toBe(false);
    await m.record("u2", "image", JOB);
    expect(await m.owner(JOB)).toBe("u1");
    expect(await m.allowed("u1", JOB.toUpperCase())).toBe(true);
    // Not a job id: nothing is stored.
    await m.record("u1", "image", "../etc/passwd");
    expect(await m.owner("../etc/passwd")).toBeNull();
  });

  test("jobs from before the record stay reachable unless strict", async () => {
    const old = "11111111-2222-4333-8444-555555555555";
    expect(await createMediaStore(memSql()).allowed("u1", old)).toBe(true);
    expect(await createMediaStore(memSql(), { strict: true }).allowed("u1", old)).toBe(false);
  });
});

describe("songs and albums pass the content policy", () => {
  const user = { id: "u_2", adult_verified_at: null };
  test("an explicit prompt or lyric is refused like an image prompt", () => {
    const ok = parseMusicRequest({ prompt: "lo-fi hip hop for studying", lyrics: "rain on the window" });
    if (typeof ok === "string") throw new Error(ok);
    expect(gateGeneration(user, musicPolicyRequest(ok))).toEqual({ ok: true });
    const explicit = "classical nude figure study of an adult";
    const byPrompt = gateGeneration(user, musicPolicyRequest({ prompt: explicit, lyrics: "" }));
    expect(byPrompt.ok).toBe(false);
    // The lyrics are read too, not just the style line.
    const byLyrics = gateGeneration(user, musicPolicyRequest({ prompt: "soft piano", lyrics: explicit }));
    expect(byLyrics.ok || byLyrics.code).toBe("explicit_requires_adult");
    const album = parseAlbumRequest({ prompt: explicit });
    if (typeof album === "string") throw new Error(album);
    expect(gateGeneration(user, musicPolicyRequest(album)).ok).toBe(false);
  });
});

describe("hosts the server will fetch", () => {
  test("a trailing dot does not get an internal name past the guard", () => {
    for (const u of ["https://localhost./", "https://foo.internal./", "https://metadata.google.internal./", "https://printer.local./"]) {
      expect(() => checkPluginUrl(u)).toThrow(PluginUrlError);
    }
    expect(checkPluginUrl("https://mcp.example.com./x").hostname).toBe("mcp.example.com.");
    expect(isPublicHostname("example.com")).toBe(true);
    expect(isPublicHostname("localhost.")).toBe(false);
    expect(isPublicHostname("intranet")).toBe(false);
    expect(isPublicHostname("[::1]")).toBe(false);
  });

  test("local NAT64, Teredo and the discard prefix are not public", () => {
    expect(isBlockedAddress("64:ff9b:1::a00:1")).toBe(true);
    expect(isBlockedAddress("2001:0:53aa:64c:0:7c2a:3f57:fefd")).toBe(true);
    expect(isBlockedAddress("100::1")).toBe(true);
    expect(isBlockedAddress("2606:4700:4700::1111")).toBe(false);
    expect(isBlockedAddress("2001:4860:4860::8888")).toBe(false);
  });

  test("a promo is never made of a loopback, private or metadata address", () => {
    for (const url of ["http://127.0.0.1/", "http://169.254.169.254/latest/meta-data/", "http://10.0.0.5:8080/", "http://localhost./", "http://[::1]/", "http://100.64.0.1/", "http://nas.local/", "http://example.com./"]) {
      expect(parsePublicPromoRequest({ url })).toBe("url must name a public host");
    }
    const ok = parsePublicPromoRequest({ url: "c0mpute.com" });
    expect(typeof ok === "string" ? ok : ok.url).toBe("https://c0mpute.com/");
    expect(typeof parsePublicPromoRequest({ url: "http://8.8.8.8/" })).toBe("object");
  });
});

describe("the MCP endpoint", () => {
  const upstream: Upstream = async () => Response.json({});
  const ctx = { user: { id: "u1", did: "d", name: null }, upstream };
  test("a message that is not an object is an invalid request, not a crash", async () => {
    for (const bad of [null, 7, "x", [1]]) {
      expect(await handleRpc(bad as never, ctx)).toMatchObject({ error: { code: -32600 } });
    }
  });

  test("promo_status only answers for a job this account started", async () => {
    const m = createMediaStore(memSql());
    const jobs = { record: (u: string, k: "promo", j: unknown) => m.record(u, k, j), owns: (u: string, j: string) => m.allowed(u, j) };
    await m.record("someone-else", "promo", JOB);
    const r = (await handleRpc({ id: 1, method: "tools/call", params: { name: "promo_status", arguments: { id: JOB } } }, { ...ctx, jobs })) as {
      result: { isError?: boolean; content: { text: string }[] };
    };
    expect(r.result.isError).toBe(true);
    expect(r.result.content[0]!.text).toBe("unknown promo job");
  });
});

describe("server helpers", () => {
  test("a plain INSERT is never sent twice; everything else may be", () => {
    expect(safeToRetry("SELECT 1")).toBe(true);
    expect(safeToRetry("UPDATE t SET a = 1")).toBe(true);
    expect(safeToRetry("DELETE FROM t")).toBe(true);
    expect(safeToRetry("CREATE TABLE IF NOT EXISTS t (a)")).toBe(true);
    expect(safeToRetry("INSERT OR REPLACE INTO t VALUES (1)")).toBe(true);
    expect(safeToRetry("insert or ignore into t values (1)")).toBe(true);
    expect(safeToRetry("INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO NOTHING")).toBe(true);
    expect(safeToRetry("  INSERT INTO t (a) VALUES (1)")).toBe(false);
  });

  test("an IPv6 client is limited by its /64, an IPv4 one by its address", () => {
    expect(ipBucket("2001:db8:1:2:aaaa:bbbb:cccc:dddd")).toBe(ipBucket("2001:db8:1:2::1"));
    expect(ipBucket("2001:db8:1:2::1")).toBe("2001:db8:1:2::/64");
    expect(ipBucket("2001:db8:1:3::1")).not.toBe(ipBucket("2001:db8:1:2::1"));
    expect(ipBucket("203.0.113.9")).toBe("203.0.113.9");
    expect(ipBucket("unknown")).toBe("unknown");
  });

  test("the limiter forgets expired windows and never grows past its cap", () => {
    const t0 = Date.now() + 10 * 3_600_000;
    for (let i = 0; i < 200; i++) rateLimit(`sweep-test:${i}`, 5, t0);
    const before = rateWindows();
    expect(before).toBeGreaterThanOrEqual(200);
    // Two hours on: the first call sweeps every window that ended.
    rateLimit("sweep-test:later", 5, t0 + 2 * 3_600_000);
    expect(rateWindows()).toBeLessThan(before - 150);
    expect(rateWindows()).toBeLessThanOrEqual(RATE_WINDOWS_MAX);
    // The limit itself still holds within a window.
    const t1 = t0 + 3 * 3_600_000;
    for (let i = 0; i < 5; i++) expect(rateLimit("sweep-test:cap", 5, t1).ok).toBe(true);
    expect(rateLimit("sweep-test:cap", 5, t1).ok).toBe(false);
  });

  test("a cookie write is same-origin only from the site itself", () => {
    const post = (headers: Record<string, string>) => new Request("http://localhost:3000/api/v1/keys", { method: "POST", headers });
    const prev = process.env.NODE_ENV;
    try {
      (process.env as Record<string, string>).NODE_ENV = "production";
      expect(sameOrigin(post({ origin: "https://c0mpute.com" }))).toBe(true);
      // The proxy's internal origin is not the site.
      expect(sameOrigin(post({ origin: "http://localhost:3000" }))).toBe(false);
      expect(sameOrigin(post({ origin: "https://evil.example" }))).toBe(false);
      expect(sameOrigin(post({ origin: "null" }))).toBe(false);
      expect(sameOrigin(post({ authorization: "Bearer c0k_x" }))).toBe(true);
    } finally {
      (process.env as Record<string, string | undefined>).NODE_ENV = prev;
    }
  });

  test("a header timeout does not outlive the headers", async () => {
    const quick = headerTimeout(10);
    quick.clear();
    await new Promise((r) => setTimeout(r, 30));
    expect(quick.signal.aborted).toBe(false);
    const slow = headerTimeout(10);
    await new Promise((r) => setTimeout(r, 30));
    expect(slow.signal.aborted).toBe(true);
  });

  test("sync never replaces a conversation with an older server copy", () => {
    expect(serverIsNewer(undefined, { updated: 5 })).toBe(true);
    expect(serverIsNewer({ updated: 5 }, { updated: 9 })).toBe(true);
    expect(serverIsNewer({ updated: 9 }, { updated: 5 })).toBe(false);
  });
});
