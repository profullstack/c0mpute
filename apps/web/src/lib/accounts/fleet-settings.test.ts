// Stage 2 of fleet management: node settings as signed commands, and the state
// nodes report on their signed polls.
import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { createPublicKey, generateKeyPairSync, sign, verify, type KeyObject } from "node:crypto";
import { createStore, type Sql } from "../chat/store";
import { createEarningsStore } from "../earnings/store";
import { buildFleet, cleanNodeState, commandMessage, createFleetService, nodeArgs, pollMessage, runAction, sha256Hex } from "./fleet";
import { createLinkService } from "./link";
import { checkNodeRequest } from "./node-auth";
import { createAccountStore } from "./store";

function memSql(db = new Database(":memory:")): Sql {
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}
const SITE = "https://c0mpute.com";
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

async function linkedEnv() {
  const sql = memSql();
  const users = createStore(sql);
  const accounts = createAccountStore(sql);
  const earnings = createEarningsStore(sql);
  const fleet = createFleetService({ sql, accounts, site: SITE });
  const link = createLinkService({ accounts, earnings, site: SITE });
  const u = await users.createLocalUser("op");
  const n = node();
  const me = { kind: "user" as const, userId: u.id, via: "key" as const };
  const ch = await link.challenge(me, n.peer);
  await link.complete(me, { challenge_id: ch.challenge_id, peer_id: n.peer, signature: n.sign(ch.message) });
  const view = async () => {
    const st = await fleet.state(u.id);
    return buildFleet({ claims: await earnings.claims(u.id), meta: await accounts.nodeMeta(u.id), board: [], t24: new Map(), t30: new Map(), tAll: new Map(), ...st });
  };
  return { sql, users, accounts, earnings, fleet, link, u, n, view };
}

const STATE = {
  v: 1,
  version: "0.3.106",
  paused: false,
  settings: { models: ["qwen3:4b"], house_models: true, uncensored: false, hosting: false, storage: null },
  effective: { served_models: ["qwen3:4b"], house_models: true, uncensored: false, hosting: false, storage: true },
  disk: { free_bytes: 40e9, total_bytes: 500e9 },
  update_error: null,
};

describe("node setting args", () => {
  test("models take a list of ids; empty means every model", () => {
    expect(nodeArgs("models", { models: ["qwen3:4b", " llama3:8b ", "qwen3:4b"] })).toEqual({ models: ["qwen3:4b", "llama3:8b"] });
    expect(nodeArgs("models", { models: "a:1b,b:2b" })).toEqual({ models: ["a:1b", "b:2b"] });
    expect(nodeArgs("models", {})).toEqual({ models: [] });
    expect(typeof nodeArgs("models", { models: ["rm -rf /"] })).toBe("string");
    expect(typeof nodeArgs("models", { models: Array.from({ length: 65 }, (_, i) => `m${i}`) })).toBe("string");
    expect(typeof nodeArgs("models", { models: 5 })).toBe("string");
  });
  test("toggles take on: true, false or null", () => {
    expect(nodeArgs("hosting", { on: false })).toEqual({ on: false });
    expect(nodeArgs("storage", { on: "on" })).toEqual({ on: true });
    expect(nodeArgs("uncensored", { on: null })).toEqual({ on: null });
    expect(typeof nodeArgs("house-models", {})).toBe("string");
    expect(typeof nodeArgs("house-models", { on: "maybe" })).toBe("string");
    expect(nodeArgs("pause", { anything: 1 })).toEqual({});
  });
});

describe("settings commands", () => {
  test("a toggle is queued with its args inside the signed text, so a node refuses any change to them", async () => {
    const env = await linkedEnv();
    const r = await runAction({ userId: env.u.id, actor: "test", action: "hosting", args: { on: false }, targets: (await env.view()).nodes, fleet: env.fleet, accounts: env.accounts, earnings: env.earnings });
    expect("results" in r && r.ok).toBe(1);
    const c = (await env.fleet.forNode(env.n.peer)).commands[0];
    expect(c.action).toBe("hosting");
    expect(JSON.parse(c.args)).toEqual({ on: false });
    const pub = createPublicKey({ key: Buffer.concat([Buffer.from("302a300506032b6570032100", "hex"), Buffer.from(await env.fleet.publicKey(), "base64")]), format: "der", type: "spki" });
    expect(verify(null, Buffer.from(commandMessage(c)), pub, Buffer.from(c.sig, "base64"))).toBe(true);
    expect(verify(null, Buffer.from(commandMessage({ ...c, args: JSON.stringify({ on: true }) })), pub, Buffer.from(c.sig, "base64"))).toBe(false);
    // audited with the value set
    const log = await env.accounts.auditLog(env.u.id);
    expect(log.some((e) => e.action === "hosting" && JSON.stringify(e.args).includes("false"))).toBe(true);
  });

  test("bad args refuse the whole action and queue nothing; a newer value supersedes one still waiting", async () => {
    const env = await linkedEnv();
    const targets = (await env.view()).nodes;
    const bad = await runAction({ userId: env.u.id, actor: "test", action: "models", args: { models: ["bad id"] }, targets, fleet: env.fleet, accounts: env.accounts, earnings: env.earnings });
    expect("error" in bad).toBe(true);
    await runAction({ userId: env.u.id, actor: "test", action: "models", args: { models: ["a:1b"] }, targets, fleet: env.fleet, accounts: env.accounts, earnings: env.earnings });
    await runAction({ userId: env.u.id, actor: "test", action: "models", args: { models: ["b:2b"] }, targets, fleet: env.fleet, accounts: env.accounts, earnings: env.earnings });
    const got = (await env.fleet.forNode(env.n.peer)).commands;
    expect(got).toHaveLength(1);
    expect(JSON.parse(got[0].args)).toEqual({ models: ["b:2b"] });
  });

  test("bulk by tag reaches every tagged node and nothing else", async () => {
    const env = await linkedEnv();
    const second = node();
    const me = { kind: "user" as const, userId: env.u.id, via: "key" as const };
    const ch = await env.link.challenge(me, second.peer);
    await env.link.complete(me, { challenge_id: ch.challenge_id, peer_id: second.peer, signature: second.sign(ch.message) });
    await env.accounts.setTags(env.u.id, env.n.peer.slice(-8), ["rack-a"], "set");
    const { selectNodes } = await import("./fleet");
    const targets = selectNodes((await env.view()).nodes, { tag: "rack-a" });
    expect(Array.isArray(targets) && targets.map((t) => t.node)).toEqual([env.n.peer.slice(-8)]);
    await runAction({ userId: env.u.id, actor: "test", action: "storage", args: { on: true }, targets: targets as never, fleet: env.fleet, accounts: env.accounts, earnings: env.earnings });
    expect((await env.fleet.forNode(env.n.peer)).commands).toHaveLength(1);
    expect((await env.fleet.forNode(second.peer)).commands).toHaveLength(0);
  });
});

describe("node state on the poll", () => {
  test("the state's hash is in the signed poll; a changed state fails the signature", () => {
    const n = node();
    const ts = Math.floor(Date.now() / 1000);
    const state = JSON.stringify(STATE);
    const msg = pollMessage({ site: SITE, peerId: n.peer, ts, kind: "poll", stateSha256: sha256Hex(state) });
    expect(msg.endsWith(`\nstate: ${sha256Hex(state)}`)).toBe(true);
    // sha256("{}") matches the node's poll_message_with_state test
    expect(sha256Hex("{}")).toBe("44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a");
    const body = { peer_id: n.peer, ts, state, signature: n.sign(msg) };
    expect(checkNodeRequest(SITE, body, "poll")).toBeNull();
    expect(checkNodeRequest(SITE, { ...body, state: state.replace("40000000000", "90000000000") }, "poll")).toContain("signature");
    // an old node's poll (no state) still checks out
    const old = pollMessage({ site: SITE, peerId: n.peer, ts, kind: "poll" });
    expect(checkNodeRequest(SITE, { peer_id: n.peer, ts, signature: n.sign(old) }, "poll")).toBeNull();
    // state signed as absent but sent anyway
    expect(checkNodeRequest(SITE, { peer_id: n.peer, ts, signature: n.sign(old), state }, "poll")).toContain("signature");
    expect(checkNodeRequest(SITE, { ...body, state: "x".repeat(20_000) }, "poll")).toContain("16 KB");
  });

  test("only a linked node's state is kept, cleaned, and shown in the fleet view", async () => {
    const env = await linkedEnv();
    expect(await env.fleet.saveState(node().peer, cleanNodeState(STATE)!)).toBe(false);
    expect(await env.fleet.saveState(env.n.peer, cleanNodeState({ ...STATE, junk: "x", settings: { ...STATE.settings, models: ["ok:1b", "not ok"] } })!)).toBe(true);
    const f = (await env.view()).nodes[0];
    expect(f.node_state?.settings.models).toEqual(["ok:1b"]);
    expect(f.node_state?.effective.hosting).toBe(false);
    expect(f.node_state?.disk?.free_bytes).toBe(40e9);
    expect(f.paused).toBe(false);
    expect(f.state_at).not.toBeNull();
    expect(JSON.stringify(f.node_state)).not.toContain("junk");
    expect(cleanNodeState("nope")).toBeNull();
    expect(cleanNodeState({ disk: { free_bytes: -1, total_bytes: 5 } })?.disk).toBeNull();
    // unlink forgets it
    await runAction({ userId: env.u.id, actor: "test", action: "unlink", args: {}, targets: (await env.view()).nodes, fleet: env.fleet, accounts: env.accounts, earnings: env.earnings });
    expect((await env.fleet.state(env.u.id)).states.size).toBe(0);
  });

  test("the last update result per node feeds the update-failed alert", async () => {
    const env = await linkedEnv();
    const targets = (await env.view()).nodes;
    await runAction({ userId: env.u.id, actor: "test", action: "update", args: {}, targets, fleet: env.fleet, accounts: env.accounts, earnings: env.earnings });
    const c = (await env.fleet.forNode(env.n.peer)).commands[0];
    await env.fleet.report(env.n.peer, c.id, false, "update failed: disk full");
    const u = await env.fleet.lastUpdates(env.u.id);
    expect(u.get(env.n.peer.slice(-8))).toMatchObject({ status: "failed", result: "update failed: disk full" });
  });
});
