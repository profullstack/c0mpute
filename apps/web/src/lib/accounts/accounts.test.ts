import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { createPublicKey, generateKeyPairSync, sign, verify, type KeyObject } from "node:crypto";
import { createStore, CoinpayTakenError, NoDidError, type Sql, type User } from "../chat/store";
import { rateLimit } from "../chat/server";
import { createEarningsStore } from "../earnings/store";
import { handleCallback, readSaved } from "./coinpay-callback";
import { buildFleet, commandMessage, createFleetService, publicKeyB64, runAction, selectNodes, signingKey } from "./fleet";
import { createLinkService, type Linker } from "./link";
import { checkNodeRequest } from "./node-auth";
import { createPasskeys, PasskeyError, relyingParty, type WebAuthnLib } from "./passkeys";
import { hashPassword, passwordProblem, validUsername, verifyPassword } from "./password";
import { base58Decode, linkMessage, looksLikePeerId, peerPublicKey, verifyPeerSignature } from "./peer";
import { cleanTags, createAccountStore, parseTtl } from "./store";
import { safeNext } from "./util";
import { pollMessage } from "./fleet";

function memSql(db = new Database(":memory:")): Sql {
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}

const DID = "did:key:z6MkTestUserOne";
const SITE = "https://c0mpute.com";

// ── a node identity, as libp2p makes it ─────────────────────────────────────
const B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
function b58(bytes: Uint8Array): string {
  let n = 0n;
  for (const b of bytes) n = n * 256n + BigInt(b);
  let s = "";
  while (n > 0n) {
    s = B58[Number(n % 58n)] + s;
    n /= 58n;
  }
  for (const b of bytes) {
    if (b !== 0) break;
    s = "1" + s;
  }
  return s;
}
function node(): { peer: string; key: KeyObject; sign: (m: string) => string } {
  const { privateKey, publicKey } = generateKeyPairSync("ed25519");
  const raw = (publicKey.export({ format: "der", type: "spki" }) as Buffer).subarray(12);
  const peer = b58(Uint8Array.from([0x00, 0x24, 0x08, 0x01, 0x12, 0x20, ...raw]));
  return { peer, key: privateKey, sign: (m) => sign(null, Buffer.from(m), privateKey).toString("base64") };
}

async function setup() {
  const sql = memSql();
  const users = createStore(sql);
  const accounts = createAccountStore(sql);
  const earnings = createEarningsStore(sql);
  const fleet = createFleetService({ sql, accounts, site: SITE });
  const link = createLinkService({ accounts, earnings, site: SITE });
  return { sql, users, accounts, earnings, fleet, link };
}

describe("peer ids", () => {
  test("the ed25519 key comes back out of the peer id and verifies signatures", () => {
    const n = node();
    expect(n.peer.startsWith("12D3KooW")).toBe(true);
    expect(looksLikePeerId(n.peer)).toBe(true);
    expect(peerPublicKey(n.peer)?.length).toBe(32);
    expect(verifyPeerSignature(n.peer, "hello", n.sign("hello"))).toBe(true);
    expect(verifyPeerSignature(n.peer, "hello!", n.sign("hello"))).toBe(false);
    expect(verifyPeerSignature(n.peer, "hello", node().sign("hello"))).toBe(false);
    expect(verifyPeerSignature(n.peer, "hello", "not base64 at all !!")).toBe(false);
    expect(looksLikePeerId("QmYyQSo1c1Ym7orWxLYvCrM2EmxFTANf8wXmmE7DWjhx5N")).toBe(false);
    expect(base58Decode("0OIl")).toBeNull();
  });
});

describe("accounts table", () => {
  test("the old CoinPay-only table is rebuilt once, keeping every id, sub, DID and session", async () => {
    const db = new Database(":memory:");
    const sql = memSql(db);
    db.run(`CREATE TABLE chat_users (id TEXT PRIMARY KEY, coinpay_sub TEXT NOT NULL UNIQUE, did TEXT NOT NULL, name TEXT, created_at TEXT NOT NULL, last_login_at TEXT NOT NULL, adult_verified_at TEXT)`);
    db.run(`INSERT INTO chat_users VALUES ('u_old', 'sub-old', '${DID}', 'Old', '2026-11-01', '2026-11-02', NULL)`);
    db.run(`CREATE TABLE chat_tokens (token_hash TEXT PRIMARY KEY, user_id TEXT NOT NULL, kind TEXT NOT NULL, label TEXT, prefix TEXT, created_at TEXT NOT NULL, expires_at TEXT, last_used_at TEXT)`);
    const s = createStore(sql);
    const key = await s.createKey("u_old", "old cli");
    const me = await s.userForToken(key);
    expect(me?.id).toBe("u_old");
    expect(me?.coinpay_sub).toBe("sub-old");
    expect((await s.upsertCoinpayUser({ sub: "sub-old", did: DID })).id).toBe("u_old");
    const notnull = db.query(`PRAGMA table_info(chat_users)`).all() as { name: string; notnull: number }[];
    expect(notnull.find((c) => c.name === "coinpay_sub")?.notnull).toBe(0);
    expect((db.query(`SELECT COUNT(*) AS n FROM chat_users_pre_accounts`).get() as { n: number }).n).toBe(1);
    // passkey/password accounts now fit
    const local = await s.createLocalUser("local");
    expect(local.coinpay_sub).toBeNull();
    expect((await s.userForToken(await s.createSession(local.id)))?.id).toBe(local.id);
  });

  test("the password hash never leaves the store", async () => {
    const { users } = await setup();
    const u = await users.createLocalUser("x");
    await users.setPassword(u.id, "alice", "$argon2id$fake");
    const me = (await users.userForToken(await users.createSession(u.id))) as Record<string, unknown>;
    expect("password_hash" in me).toBe(false);
    expect("password_hash" in ((await users.userById(u.id)) as Record<string, unknown>)).toBe(false);
  });
});

describe("binding CoinPay by sub, never by email", () => {
  test("sign-in binds by sub; a second account with the same email is a different account", async () => {
    const { users } = await setup();
    const a = await users.upsertCoinpayUser({ sub: "sub-a", did: DID, email: "same@x.com" } as never);
    const b = await users.upsertCoinpayUser({ sub: "sub-b", did: DID, email: "same@x.com" } as never);
    expect(a.id).not.toBe(b.id);
    expect((await users.upsertCoinpayUser({ sub: "sub-a", did: "did:key:z6MkOther" })).id).toBe(a.id);
  });

  test("connect CoinPay to a passkey account; a sub owned elsewhere is refused, never merged", async () => {
    const { users } = await setup();
    const owner = await users.upsertCoinpayUser({ sub: "sub-taken", did: DID });
    const local = await users.createLocalUser("passkey person");
    await expect(users.linkCoinpay(local.id, { sub: "sub-taken", did: DID })).rejects.toBeInstanceOf(CoinpayTakenError);
    expect((await users.userById(owner.id))?.coinpay_sub).toBe("sub-taken");
    expect((await users.userById(local.id))?.coinpay_sub).toBeNull();
    await expect(users.linkCoinpay(local.id, { sub: "sub-new" })).rejects.toBeInstanceOf(NoDidError);
    const linked = await users.linkCoinpay(local.id, { sub: "sub-new", did: DID });
    expect(linked.coinpay_sub).toBe("sub-new");
    // now CoinPay sign-in lands in the same account
    expect((await users.upsertCoinpayUser({ sub: "sub-new", did: DID })).id).toBe(local.id);
    // and it cannot be re-pointed at another CoinPay account
    await expect(users.linkCoinpay(local.id, { sub: "sub-other", did: DID })).rejects.toBeInstanceOf(CoinpayTakenError);
  });
});

describe("CoinPay callback (OAuth 2.1 code + PKCE + state)", () => {
  const deps = (users: ReturnType<typeof createStore>, over: Partial<Parameters<typeof handleCallback>[2]> = {}) => ({
    users,
    exchange: async (_code: string, verifier: string) => (verifier === "right-verifier" ? "access-token" : Promise.reject(new Error("invalid_grant: PKCE verification failed"))),
    userinfo: async () => ({ sub: "sub-cb", did: DID, name: "CB" }),
    current: null as User | null,
    ...over,
  });

  test("state mismatch, missing state and missing cookie are refused before any token exchange", async () => {
    const { users } = await setup();
    let exchanged = 0;
    const d = deps(users, { exchange: async () => (exchanged++, "t") });
    for (const [q, saved] of [
      ["?code=c&state=evil", { state: "good", verifier: "right-verifier" }],
      ["?code=c", { state: "good", verifier: "right-verifier" }],
      ["?code=c&state=good", {}],
      ["?state=good", { state: "good", verifier: "right-verifier" }],
    ] as const) {
      const r = await handleCallback(new URL(`${SITE}/api/auth/coinpay/callback${q}`), saved, d);
      expect(r.user).toBeNull();
      expect(r.redirect).toContain("error=state");
    }
    expect(exchanged).toBe(0);
  });

  test("the stored verifier is what is exchanged; a verifier mismatch signs nobody in", async () => {
    const { users } = await setup();
    const bad = await handleCallback(new URL(`${SITE}/cb?code=c&state=s`), { state: "s", verifier: "wrong-verifier" }, deps(users));
    expect(bad.user).toBeNull();
    expect(bad.redirect).toContain("error=signin");
    const good = await handleCallback(new URL(`${SITE}/cb?code=c&state=s`), { state: "s", verifier: "right-verifier", next: "/dashboard/nodes" }, deps(users));
    expect(good.user?.coinpay_sub).toBe("sub-cb");
    expect(good.redirect).toBe("/dashboard/nodes");
  });

  test("no DID: a friendly error, no account", async () => {
    const { users } = await setup();
    const r = await handleCallback(new URL(`${SITE}/cb?code=c&state=s`), { state: "s", verifier: "right-verifier" }, deps(users, { userinfo: async () => ({ sub: "nodid" }) }));
    expect(r.redirect).toContain("error=no_did");
    expect(r.user).toBeNull();
  });

  test("link mode attaches to the signed-in account and starts no new session; taken subs refused", async () => {
    const { users } = await setup();
    const me = await users.createLocalUser("me");
    const r = await handleCallback(new URL(`${SITE}/cb?code=c&state=s`), { state: "s", verifier: "right-verifier", link: true, next: "/account" }, deps(users, { current: me }));
    expect(r.user).toBeNull();
    expect(r.redirect).toBe("/account?connected=coinpay");
    expect((await users.userById(me.id))?.coinpay_sub).toBe("sub-cb");
    const other = await users.createLocalUser("other");
    const r2 = await handleCallback(new URL(`${SITE}/cb?code=c&state=s`), { state: "s", verifier: "right-verifier", link: true }, deps(users, { current: other }));
    expect(r2.redirect).toContain("error=coinpay_taken");
    const r3 = await handleCallback(new URL(`${SITE}/cb?code=c&state=s`), { state: "s", verifier: "right-verifier", link: true }, deps(users, { current: null }));
    expect(r3.redirect).toContain("error=session");
  });

  test("open redirects are refused", () => {
    expect(safeNext("//evil.com")).toBe("/account");
    expect(safeNext("/\\evil.com")).toBe("/account");
    expect(safeNext("https://evil.com")).toBe("/account");
    expect(safeNext("/fleet?x=1")).toBe("/fleet?x=1");
    expect(readSaved("a=b; c0_oauth=%7B%22state%22%3A%22s%22%7D").state).toBe("s");
    expect(readSaved("c0_oauth=garbage")).toEqual({});
  });
});

describe("CLI device flow", () => {
  test("a device code is approved once and redeemed once; replays get nothing", async () => {
    const { users } = await setup();
    const a = await users.createLocalUser("a"); // any sign-in method may approve
    const b = await users.upsertCoinpayUser({ sub: "b", did: DID });
    const d = await users.startDevice("c0mpute cli on test");
    expect((await users.pollDevice(d.device_code)).status).toBe("pending");
    expect(await users.approveDevice(d.user_code, a.id)).toBe(true);
    expect(await users.approveDevice(d.user_code, b.id)).toBe(false);
    const first = await users.pollDevice(d.device_code);
    expect(first.status).toBe("ok");
    expect((await users.pollDevice(d.device_code)).status).toBe("expired");
    const key = (first as { key: string }).key;
    expect((await users.userForToken(key))?.id).toBe(a.id);
    expect(await users.approveDevice(d.user_code, a.id)).toBe(false);
  });
});

describe("node link", () => {
  async function linkOnce(env: Awaited<ReturnType<typeof setup>>, linker: Linker, n = node()) {
    const ch = await env.link.challenge(linker, n.peer);
    return { n, ch, r: await env.link.complete(linker, { challenge_id: ch.challenge_id, peer_id: n.peer, signature: n.sign(ch.message), info: { hostname: "box1", cores: 8 } }) };
  }

  test("a signed challenge links and verifies the node", async () => {
    const env = await setup();
    const u = await env.users.createLocalUser("op");
    const { n, ch, r } = await linkOnce(env, { kind: "user", userId: u.id, via: "key" });
    expect(ch.message).toBe(linkMessage({ site: SITE, accountId: u.id, peerId: n.peer, challenge: ch.message.split("challenge: ")[1] }));
    expect(r.verified).toBe(true);
    expect(r.node.via).toBe("signed");
    expect((await env.accounts.nodeMeta(u.id))[0].peer_id).toBe(n.peer);
    expect((await env.accounts.auditLog(u.id))[0].action).toBe("link");
  });

  test("replayed challenge, forged signature, another account's challenge and another site's message all fail", async () => {
    const env = await setup();
    const u = await env.users.createLocalUser("op");
    const eve = await env.users.createLocalUser("eve");
    const me: Linker = { kind: "user", userId: u.id, via: "key" };
    const n = node();
    // replay
    const ch = await env.link.challenge(me, n.peer);
    const sig = n.sign(ch.message);
    await env.link.complete(me, { challenge_id: ch.challenge_id, peer_id: n.peer, signature: sig });
    await expect(env.link.complete(me, { challenge_id: ch.challenge_id, peer_id: n.peer, signature: sig })).rejects.toMatchObject({ status: 410 });
    // forged: signed by another key for this peer id
    const ch2 = await env.link.challenge(me, n.peer);
    await expect(env.link.complete(me, { challenge_id: ch2.challenge_id, peer_id: n.peer, signature: node().sign(ch2.message) })).rejects.toMatchObject({ status: 401 });
    // the forged try burned the challenge
    await expect(env.link.complete(me, { challenge_id: ch2.challenge_id, peer_id: n.peer, signature: n.sign(ch2.message) })).rejects.toMatchObject({ status: 410 });
    // a challenge issued to me, answered by eve
    const ch3 = await env.link.challenge(me, n.peer);
    await expect(env.link.complete({ kind: "user", userId: eve.id, via: "key" }, { challenge_id: ch3.challenge_id, peer_id: n.peer, signature: n.sign(ch3.message) })).rejects.toMatchObject({ status: 403 });
    // a signature over another site's message
    const ch4 = await env.link.challenge(me, n.peer);
    const other = ch4.message.replace(SITE, "https://evil.example");
    await expect(env.link.complete(me, { challenge_id: ch4.challenge_id, peer_id: n.peer, signature: n.sign(other) })).rejects.toMatchObject({ status: 401 });
    // challenge for one node, answered for another
    const n2 = node();
    const ch5 = await env.link.challenge(me, n.peer);
    await expect(env.link.complete(me, { challenge_id: ch5.challenge_id, peer_id: n2.peer, signature: n2.sign(ch5.message) })).rejects.toMatchObject({ status: 403 });
  });

  test("a newer signed link moves the node; the old account loses it", async () => {
    const env = await setup();
    const a = await env.users.createLocalUser("a");
    const b = await env.users.createLocalUser("b");
    const n = node();
    await linkOnce(env, { kind: "user", userId: a.id, via: "key" }, n);
    await linkOnce(env, { kind: "user", userId: b.id, via: "key" }, n);
    expect(await env.accounts.nodeMeta(a.id)).toHaveLength(0);
    expect(await env.earnings.claims(a.id)).toHaveLength(0);
    expect((await env.accounts.verifiedOwner(n.peer.slice(-8)))?.user_id).toBe(b.id);
  });

  test("enrollment tokens: link headlessly with tags; revoked, expired and used-up tokens stop working", async () => {
    const env = await setup();
    const u = await env.users.createLocalUser("fleet");
    const { token, info } = await env.accounts.createEnrollToken(u.id, { name: "rack-a", ttlMs: 7 * 86_400_000, maxUses: 2, tags: ["rack-a"] });
    expect(token.startsWith("c0e_")).toBe(true);
    expect(info.prefix).toBe(token.slice(0, 10));
    const owner = await env.accounts.enrollTokenOwner(token);
    expect(owner?.user_id).toBe(u.id);
    const linker: Linker = { kind: "enroll", userId: u.id, tokenHash: owner!.hash, tokenName: owner!.name, tags: owner!.tags };
    await linkOnce(env, linker);
    await linkOnce(env, linker);
    expect((await env.accounts.nodeMeta(u.id)).every((m) => m.tags.includes("rack-a") && m.via === "token")).toBe(true);
    expect(await env.accounts.enrollTokenOwner(token)).toBeNull(); // used up
    const t2 = await env.accounts.createEnrollToken(u.id, { name: "b", ttlMs: null });
    expect(await env.accounts.revokeEnrollToken(u.id, "b")).toBe(1);
    expect(await env.accounts.enrollTokenOwner(t2.token)).toBeNull();
    // stored hashed: the secret is nowhere in the table
    const rows = await env.sql.all(`SELECT * FROM fleet_enroll_tokens`);
    expect(JSON.stringify(rows)).not.toContain(token);
    // a challenge issued through a token cannot be finished with a user key
    const n = node();
    const t3 = await env.accounts.createEnrollToken(u.id, { name: "c", ttlMs: null });
    const o3 = (await env.accounts.enrollTokenOwner(t3.token))!;
    const ch = await env.link.challenge({ kind: "enroll", userId: u.id, tokenHash: o3.hash, tokenName: "c", tags: [] }, n.peer);
    await expect(env.link.complete({ kind: "user", userId: u.id, via: "key" }, { challenge_id: ch.challenge_id, peer_id: n.peer, signature: n.sign(ch.message) })).rejects.toMatchObject({ status: 403 });
  });

  test("ttl and tags parse strictly", () => {
    expect(parseTtl("7d")).toBe(7 * 86_400_000);
    expect(parseTtl("never")).toBeNull();
    expect(parseTtl("400d")).toBe("invalid");
    expect(parseTtl("7 weeks")).toBe("invalid");
    expect(cleanTags("Vast, home,rack-a")).toEqual(["vast", "home", "rack-a"]);
    expect(cleanTags(["bad tag"])).toBeNull();
  });
});

describe("fleet commands", () => {
  test("only verified nodes get commands; each is signed for this account and node; only that node can report", async () => {
    const env = await setup();
    const u = await env.users.createLocalUser("op");
    const n = node();
    const ch = await env.link.challenge({ kind: "user", userId: u.id, via: "key" }, n.peer);
    await env.link.complete({ kind: "user", userId: u.id, via: "key" }, { challenge_id: ch.challenge_id, peer_id: n.peer, signature: n.sign(ch.message) });
    // an unverified claim (typed into the browser) next to it
    await env.earnings.claim(u.id, { node: "ABCDEFGH", via: "web" });
    const fleet = buildFleet({ claims: await env.earnings.claims(u.id), meta: await env.accounts.nodeMeta(u.id), board: [], t24: new Map(), t30: new Map(), tAll: new Map() });
    expect(fleet.totals.nodes).toBe(2);
    expect(fleet.totals.verified).toBe(1);
    const r = await runAction({ userId: u.id, actor: "test", action: "pause", args: {}, targets: fleet.nodes, fleet: env.fleet, accounts: env.accounts, earnings: env.earnings });
    expect("results" in r && r.ok).toBe(1);
    expect("results" in r && r.failed).toBe(1);
    const got = await env.fleet.forNode(n.peer);
    expect(got.account_id).toBe(u.id);
    expect(got.commands).toHaveLength(1);
    const c = got.commands[0];
    expect(c.action).toBe("pause");
    const pub = createPublicKey({ key: Buffer.concat([Buffer.from("302a300506032b6570032100", "hex"), Buffer.from(await env.fleet.publicKey(), "base64")]), format: "der", type: "spki" });
    // the key is kept in the database: a second service on the same DB signs with the same key
    expect(await createFleetService({ sql: env.sql, accounts: env.accounts, site: SITE }).publicKey()).toBe(await env.fleet.publicKey());
    expect(verify(null, Buffer.from(commandMessage(c)), pub, Buffer.from(c.sig, "base64"))).toBe(true);
    // tampering breaks the signature
    expect(verify(null, Buffer.from(commandMessage({ ...c, action: "restart" })), pub, Buffer.from(c.sig, "base64"))).toBe(false);
    // another node cannot fetch or report it
    expect((await env.fleet.forNode(node().peer)).commands).toHaveLength(0);
    expect(await env.fleet.report(node().peer, c.id, true, "")).toBe(false);
    expect(await env.fleet.report(n.peer, c.id, true, "paused")).toBe(true);
    const st = await env.fleet.state(u.id);
    expect(st.paused.get(n.peer.slice(-8))).toBe(true);
  });

  test("pause then resume before the node polled: only resume is sent", async () => {
    const env = await setup();
    const u = await env.users.createLocalUser("op");
    const n = node();
    await env.accounts.verifyNode(u.id, { node: n.peer.slice(-8), peer_id: n.peer, via: "key" });
    await env.fleet.enqueue(u.id, { node: n.peer.slice(-8), peer_id: n.peer }, "pause");
    await env.fleet.enqueue(u.id, { node: n.peer.slice(-8), peer_id: n.peer }, "resume");
    const got = await env.fleet.forNode(n.peer);
    expect(got.commands.map((c) => c.action)).toEqual(["resume"]);
  });

  test("an unlinked node gets nothing queued under the old account", async () => {
    const env = await setup();
    const u = await env.users.createLocalUser("op");
    const n = node();
    await env.accounts.verifyNode(u.id, { node: n.peer.slice(-8), peer_id: n.peer, via: "key" });
    await env.fleet.enqueue(u.id, { node: n.peer.slice(-8), peer_id: n.peer }, "update");
    await env.accounts.unverifyNode(u.id, n.peer.slice(-8));
    expect((await env.fleet.forNode(n.peer)).commands).toHaveLength(0);
  });

  test("node requests: signature over a fresh timestamp, for this site", () => {
    const n = node();
    const now = 1_800_000_000;
    const body = { peer_id: n.peer, ts: now, signature: n.sign(pollMessage({ site: SITE, peerId: n.peer, ts: now, kind: "poll" })) };
    expect(checkNodeRequest(SITE, body, "poll", now)).toBeNull();
    expect(checkNodeRequest(SITE, body, "poll", now + 3600)).toContain("ts");
    expect(checkNodeRequest("https://evil.example", body, "poll", now)).toContain("signature");
    expect(checkNodeRequest(SITE, { ...body, signature: node().sign("x") }, "poll", now)).toContain("signature");
    const res = { peer_id: n.peer, ts: now, id: "fc_1", ok: true, signature: n.sign(pollMessage({ site: SITE, peerId: n.peer, ts: now, kind: "result", id: "fc_1", ok: true })) };
    expect(checkNodeRequest(SITE, res, "result", now)).toBeNull();
    expect(checkNodeRequest(SITE, { ...res, ok: false }, "result", now)).toContain("signature");
  });

  test("selection by ids, tag or all; bulk limits; key round trip", () => {
    const mk = (id: string, tags: string[]) => ({ node: id, tags }) as never;
    const nodes = [mk("AAAAAAAA", ["vast"]), mk("BBBBBBBB", ["home"])];
    expect(selectNodes(nodes, { tag: "vast" })).toHaveLength(1);
    expect(selectNodes(nodes, { all: true })).toHaveLength(2);
    expect(selectNodes(nodes, { nodes: ["BBBBBBBB"] })).toHaveLength(1);
    expect(selectNodes(nodes, { nodes: ["CCCCCCCC"] })).toContain("not one of your nodes");
    expect(selectNodes(nodes, {})).toContain("say which nodes");
    const k = signingKey(undefined);
    expect(Buffer.from(publicKeyB64(k), "base64").length).toBe(32);
  });
});

describe("passkeys (flow, with a stand-in authenticator check)", () => {
  // Real WebAuthn verification is @simplewebauthn/server's (tested upstream and
  // live with a virtual authenticator); here the flow around it is tested.
  function fakeLib(state: { counter: number; userVerified?: boolean }): WebAuthnLib {
    return {
      generateRegistrationOptions: (async (o: { userID: Uint8Array }) => ({ challenge: "reg-" + Math.random(), user: { id: Buffer.from(o.userID).toString("base64url") } })) as never,
      generateAuthenticationOptions: (async () => ({ challenge: "auth-" + Math.random() })) as never,
      verifyRegistrationResponse: (async (o: { response: { id: string; ok?: boolean }; expectedChallenge: string; expectedRPID: string }) => {
        if (!o.response.ok || o.expectedRPID !== "c0mpute.com") return { verified: false };
        return {
          verified: true,
          registrationInfo: { credential: { id: o.response.id, publicKey: new Uint8Array([1, 2, 3]), counter: 0, transports: ["internal"] }, credentialDeviceType: "multiDevice", credentialBackedUp: true },
        };
      }) as never,
      verifyAuthenticationResponse: (async (o: { response: { ok?: boolean } }) => ({ verified: !!o.response.ok, authenticationInfo: { newCounter: ++state.counter } })) as never,
    };
  }

  test("signed out: the first passkey creates an account; signed in: it joins that account", async () => {
    const env = await setup();
    const pk = createPasskeys({ users: env.users, accounts: env.accounts, lib: fakeLib({ counter: 0 }), rp: relyingParty(SITE) });
    const o = await pk.registerOptions(null, { name: "laptop" });
    const r = await pk.registerVerify(o.ceremony, { id: "cred-1", ok: true }, { current: null, name: "laptop" });
    expect(r.created).toBe(true);
    expect(r.user.coinpay_sub).toBeNull();
    // replaying the same ceremony fails
    await expect(pk.registerVerify(o.ceremony, { id: "cred-x", ok: true }, { current: null })).rejects.toBeInstanceOf(PasskeyError);
    // add a second passkey while signed in
    const o2 = await pk.registerOptions(r.user);
    const r2 = await pk.registerVerify(o2.ceremony, { id: "cred-2", ok: true }, { current: r.user, name: "phone" });
    expect(r2.created).toBe(false);
    expect(r2.user.id).toBe(r.user.id);
    expect(await env.accounts.passkeys(r.user.id)).toHaveLength(2);
    // a ceremony started signed in cannot finish as someone else
    const o3 = await pk.registerOptions(r.user);
    const stranger = await env.users.createLocalUser("stranger");
    await expect(pk.registerVerify(o3.ceremony, { id: "cred-3", ok: true }, { current: stranger })).rejects.toMatchObject({ status: 403 });
    // the same credential cannot be registered twice
    const o4 = await pk.registerOptions(null);
    await expect(pk.registerVerify(o4.ceremony, { id: "cred-1", ok: true }, { current: null })).rejects.toMatchObject({ status: 409 });
  });

  test("login: discoverable, unknown credentials refused, ceremony one-shot, counter updated", async () => {
    const env = await setup();
    const state = { counter: 0 };
    const pk = createPasskeys({ users: env.users, accounts: env.accounts, lib: fakeLib(state), rp: relyingParty(SITE) });
    const o = await pk.registerOptions(null);
    const { user } = await pk.registerVerify(o.ceremony, { id: "cred-L", ok: true }, { current: null });
    const l = await pk.loginOptions();
    expect((await pk.loginVerify(l.ceremony, { id: "cred-L", ok: true })).id).toBe(user.id);
    expect((await env.accounts.passkey("cred-L"))?.counter).toBe(1);
    await expect(pk.loginVerify(l.ceremony, { id: "cred-L", ok: true })).rejects.toBeInstanceOf(PasskeyError);
    const l2 = await pk.loginOptions();
    await expect(pk.loginVerify(l2.ceremony, { id: "nope", ok: true })).rejects.toMatchObject({ status: 401 });
    const l3 = await pk.loginOptions();
    await expect(pk.loginVerify(l3.ceremony, { id: "cred-L", ok: false })).rejects.toMatchObject({ status: 401 });
  });

  test("passkey account + CoinPay link, never by email; removing the last way in is the caller's check", async () => {
    const env = await setup();
    const pk = createPasskeys({ users: env.users, accounts: env.accounts, lib: fakeLib({ counter: 0 }), rp: relyingParty(SITE) });
    const o = await pk.registerOptions(null);
    const { user } = await pk.registerVerify(o.ceremony, { id: "cred-M", ok: true }, { current: null });
    await env.users.linkCoinpay(user.id, { sub: "sub-M", did: DID });
    expect((await env.users.upsertCoinpayUser({ sub: "sub-M", did: DID })).id).toBe(user.id);
  });
});

describe("deprecated passwords", () => {
  test("argon2id, rules, usernames", async () => {
    const h = await hashPassword("correct horse battery staple");
    expect(h.startsWith("$argon2id$")).toBe(true);
    expect(await verifyPassword("correct horse battery staple", h)).toBe(true);
    expect(await verifyPassword("wrong", h)).toBe(false);
    expect(await verifyPassword("x", "$2b$10$notargon")).toBe(false);
    expect(passwordProblem("short")).toContain("at least");
    expect(passwordProblem("long enough password")).toBeNull();
    expect(validUsername("alice")).toBe(true);
    expect(validUsername("a")).toBe(false);
    expect(validUsername("../etc")).toBe(false);
  });

  test("attempt limits trip per key", () => {
    const k = `pw-user:test-${Math.random()}`;
    for (let i = 0; i < 10; i++) expect(rateLimit(k, 10).ok).toBe(true);
    expect(rateLimit(k, 10).ok).toBe(false);
  });
});
