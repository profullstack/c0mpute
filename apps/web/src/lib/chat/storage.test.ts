import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { createPrivateKey, createPublicKey, diffieHellman, hkdfSync, randomBytes, verify } from "node:crypto";
import { createStore, type Sql } from "./store";
import { parseScope } from "./oauth";
import { scopedUser } from "./server";
import { createStorageStore, linkContext, objectsFor, parseIdentity, refToSiteId, replicationStatus, StorageError, TICKET_DOMAIN, ticketKey, x25519Pair } from "./storage";

function memSql(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}

const SEED = "11".repeat(32);
const X25519_SPKI = Buffer.from("302a300506032b656e032100", "hex");
const X25519_PKCS8 = Buffer.from("302e020100300506032b656e04220420", "hex");

/** What `c0mpute storage key link` computes (StorageSecret::link_proof). */
function cliProof(secret: Buffer, identityKey: Buffer, serverKeyB64: string, context: string) {
  const serverPub = Buffer.from(serverKeyB64, "base64url");
  const ss = diffieHellman({
    privateKey: createPrivateKey({ key: Buffer.concat([X25519_PKCS8, secret]), format: "der", type: "pkcs8" }),
    publicKey: createPublicKey({ key: Buffer.concat([X25519_SPKI, serverPub]), format: "der", type: "spki" }),
  });
  return Buffer.from(hkdfSync("sha256", ss, Buffer.concat([serverPub, identityKey]), Buffer.from(`c0mpute-storage-link-v1\n${context}`), 32)).toString("base64url");
}

async function setup() {
  const sql = memSql();
  const accounts = createStore(sql);
  const u = await accounts.upsertCoinpayUser({ sub: "sub-1", did: "did:key:z6Mktest" });
  const st = createStorageStore(sql, { ticketSeed: () => SEED, limits: { dailyBytes: 1_000_000_000, uploadsPerHour: 3 } });
  const me = x25519Pair();
  const identity = `c0ps1.${me.public.toString("base64url")}`;
  return { sql, accounts, u, st, me, identity };
}

async function link(st: Awaited<ReturnType<typeof setup>>) {
  const ch = await st.st.startLink(st.u.id);
  const proof = cliProof(st.me.secret, st.me.public, ch.server_key, linkContext(ch.challenge_id, ch.nonce, st.identity));
  return st.st.finishLink(st.u.id, { challenge_id: ch.challenge_id, identity: st.identity, proof, fingerprint: "aa:bb:cc:dd:ee:ff:00:11" });
}

describe("linking a storage identity proves possession of the key", () => {
  test("a correct proof links; the challenge is single use", async () => {
    const s = await setup();
    const ch = await s.st.startLink(s.u.id);
    const proof = cliProof(s.me.secret, s.me.public, ch.server_key, linkContext(ch.challenge_id, ch.nonce, s.identity));
    const linked = await s.st.finishLink(s.u.id, { challenge_id: ch.challenge_id, identity: s.identity, proof });
    expect(linked.identity).toBe(s.identity);
    expect((await s.st.identity(s.u.id))?.identity).toBe(s.identity);
    await expect(s.st.finishLink(s.u.id, { challenge_id: ch.challenge_id, identity: s.identity, proof })).rejects.toThrow(StorageError);
  });

  test("someone else's public identity cannot be linked without its key", async () => {
    const s = await setup();
    const victim = x25519Pair();
    const victimId = `c0ps1.${victim.public.toString("base64url")}`;
    const ch = await s.st.startLink(s.u.id);
    // Proof made with the attacker's own key for the victim's identity.
    const proof = cliProof(s.me.secret, victim.public, ch.server_key, linkContext(ch.challenge_id, ch.nonce, victimId));
    await expect(s.st.finishLink(s.u.id, { challenge_id: ch.challenge_id, identity: victimId, proof })).rejects.toMatchObject({ code: "bad_proof" });
    expect(await s.st.identity(s.u.id)).toBeNull();
  });

  test("another account's challenge does not work", async () => {
    const s = await setup();
    const other = await s.accounts.upsertCoinpayUser({ sub: "sub-2", did: "did:key:z6Mkother" });
    const ch = await s.st.startLink(other.id);
    const proof = cliProof(s.me.secret, s.me.public, ch.server_key, linkContext(ch.challenge_id, ch.nonce, s.identity));
    await expect(s.st.finishLink(s.u.id, { challenge_id: ch.challenge_id, identity: s.identity, proof })).rejects.toMatchObject({ code: "invalid_challenge" });
  });

  test("unlink", async () => {
    const s = await setup();
    await link(s);
    await s.st.unlink(s.u.id);
    expect(await s.st.identity(s.u.id)).toBeNull();
  });
});

describe("upload sessions", () => {
  test("need a linked identity, and sign a ticket the gateway can verify", async () => {
    const s = await setup();
    await expect(s.st.startUpload(s.u.id, { bytes: 1000 }, "PairUX")).rejects.toMatchObject({ code: "not_linked" });
    await link(s);
    const up = await s.st.startUpload(s.u.id, { bytes: 5_000_000, objects: 1 }, "PairUX");
    expect(up.identity).toBe(s.identity);
    const [payload, sig] = up.ticket.split(".");
    const pub = createPublicKey({ key: Buffer.concat([Buffer.from("302a300506032b6570032100", "hex"), Buffer.from(ticketKey(SEED)!.publicHex, "hex")]), format: "der", type: "spki" });
    expect(verify(null, Buffer.from(`${TICKET_DOMAIN}\n${payload}`), pub, Buffer.from(sig, "base64url"))).toBe(true);
    const t = JSON.parse(Buffer.from(payload, "base64url").toString());
    expect(t).toMatchObject({ v: 1, upload: up.upload_id, max_bytes: 5_000_000, max_objects: 1 });
    expect(t.sub).not.toContain(s.u.id);
  });

  test("limits: size, objects, rate", async () => {
    const s = await setup();
    await link(s);
    await expect(s.st.startUpload(s.u.id, { bytes: 5 * 1024 ** 3 }, null)).rejects.toMatchObject({ code: "too_large" });
    await expect(s.st.startUpload(s.u.id, { bytes: 1000, objects: 9 }, null)).rejects.toMatchObject({ code: "invalid_request" });
    for (let i = 0; i < 3; i++) await s.st.startUpload(s.u.id, { bytes: 1000 }, null);
    await expect(s.st.startUpload(s.u.id, { bytes: 1000 }, null)).rejects.toMatchObject({ code: "rate_limited" });
    expect(objectsFor(192 * 1024 * 1024)).toBe(2);
    expect(objectsFor(192 * 1024 * 1024 + 1)).toBe(3);
  });

  test("complete records the ref and its parts; status reads every object", async () => {
    const s = await setup();
    await link(s);
    const up = await s.st.startUpload(s.u.id, { bytes: 1000 }, "PairUX");
    const root = "0123456789abcdef0123";
    const part = "fedcba9876543210fedc";
    await s.st.completeUpload(s.u.id, up.upload_id, { ref: `c0p1:${root}`, site_ids: [part, "not-hex"] });
    const row = await s.st.uploadByRef(s.u.id, root);
    expect(row?.site_ids.sort()).toEqual([root, part].sort());
    expect((await s.st.listUploads(s.u.id)).map((u) => u.app)).toEqual(["PairUX"]);
    const other = await s.accounts.upsertCoinpayUser({ sub: "sub-3", did: "did:key:z6Mkthird" });
    expect(await s.st.uploadByRef(other.id, root)).toBeNull();
    const fake = (async (url: string) => {
      const copies = url.endsWith(root) ? 3 : 2;
      return new Response(JSON.stringify({ published: true, copies, target: 3 }));
    }) as unknown as typeof fetch;
    const live = await replicationStatus("https://gw", row!.site_ids, fake);
    expect(live).toMatchObject({ status: "replicating", copies: 2, target: 3 });
  });
});

describe("scopes", () => {
  test("parseScope", () => {
    expect(parseScope(null)).toBe("mcp");
    expect(parseScope("storage:write")).toBe("storage:write");
    expect(parseScope("storage:write mcp")).toBe("mcp storage:write");
    expect(parseScope("storage:read")).toBeNull();
  });

  test("a storage:write key works on storage routes only; old oauth keys stay mcp", async () => {
    const s = await setup();
    const st = await s.accounts.userForToken(await s.accounts.createOAuthKey(s.u.id, "PairUX (OAuth)", "storage:write"));
    expect(scopedUser(st, { scope: "storage:write" })?.id).toBe(s.u.id);
    expect(scopedUser(st, { oauth: true })).toBeNull();
    expect(scopedUser(st)).toBeNull();
    const mcp = await s.accounts.userForToken(await s.accounts.createOAuthKey(s.u.id, "Claude (OAuth)"));
    expect(scopedUser(mcp, { oauth: true })?.id).toBe(s.u.id);
    expect(scopedUser(mcp, { scope: "storage:write" })).toBeNull();
    const key = await s.accounts.userForToken(await s.accounts.createKey(s.u.id, "laptop"));
    expect(scopedUser(key, { scope: "storage:write" })?.token_kind).toBe("key");
  });
});

test("identity and ref parsing", () => {
  const k = randomBytes(32);
  expect(parseIdentity(`c0ps1.${k.toString("base64url")}`)?.equals(k)).toBe(true);
  expect(parseIdentity(`c0ps1.${Buffer.alloc(32).toString("base64url")}`)).toBeNull();
  expect(parseIdentity("c0dm1.abc")).toBeNull();
  expect(refToSiteId("c0p1:0123456789abcdef0123")).toBe("0123456789abcdef0123");
  expect(refToSiteId("c0p1%3A0123456789abcdef0123")).toBe("0123456789abcdef0123");
  expect(refToSiteId("c0p1:0123456789abcdef0123#k=secret")).toBe("0123456789abcdef0123");
  expect(refToSiteId("nope")).toBeNull();
});
