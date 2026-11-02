import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { createStore, isValidDid, NoDidError, normalizeUserCode, type Sql } from "./store";
import { parseChatRequest, type Upstream } from "./completions";
import { handleRpc } from "./mcp";

function memSql(): Sql {
  const db = new Database(":memory:");
  return {
    all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
    run: async (q, p = []) => {
      db.query(q).run(...(p as never[]));
    },
  };
}

const DID = "did:key:z6MkswFb62xmEDrqnknM3TP112AiH6A5YETp7gc2Qz4Wqkar";

describe("store", () => {
  test("refuses a CoinPay account without a DID", async () => {
    const s = createStore(memSql());
    await expect(s.upsertCoinpayUser({ sub: "a" })).rejects.toBeInstanceOf(NoDidError);
    await expect(s.upsertCoinpayUser({ sub: "a", did: "not-a-did" })).rejects.toBeInstanceOf(NoDidError);
  });

  test("binds by sub, never by email, and updates the DID", async () => {
    const s = createStore(memSql());
    const u1 = await s.upsertCoinpayUser({ sub: "sub-1", did: DID, name: "A" });
    const u2 = await s.upsertCoinpayUser({ sub: "sub-1", did: "did:key:z6Mknew", name: "A" });
    expect(u2.id).toBe(u1.id);
    expect(u2.did).toBe("did:key:z6Mknew");
    const other = await s.upsertCoinpayUser({ sub: "sub-2", did: DID });
    expect(other.id).not.toBe(u1.id);
  });

  test("sessions and keys resolve; revoked and unknown tokens do not", async () => {
    const s = createStore(memSql());
    const u = await s.upsertCoinpayUser({ sub: "x", did: DID });
    const session = await s.createSession(u.id);
    const key = await s.createKey(u.id, "laptop");
    expect(session.startsWith("c0s_")).toBe(true);
    expect(key.startsWith("c0k_")).toBe(true);
    expect((await s.userForToken(session))?.id).toBe(u.id);
    expect((await s.userForToken(key))?.token_kind).toBe("key");
    expect(await s.userForToken("c0k_nope")).toBeNull();
    expect(await s.userForToken("ifc_whatever")).toBeNull();
    const [listed] = await s.listKeys(u.id);
    expect(listed.label).toBe("laptop");
    expect(await s.revokeKeyById("someone-else", listed.id)).toBe(false);
    expect(await s.revokeKeyById(u.id, listed.id)).toBe(true);
    expect(await s.userForToken(key)).toBeNull();
    await s.revokeToken(session);
    expect(await s.userForToken(session)).toBeNull();
  });

  test("device flow: pending, approve once, key issued once", async () => {
    const s = createStore(memSql());
    const u = await s.upsertCoinpayUser({ sub: "d", did: DID });
    const d = await s.startDevice("my laptop");
    expect(d.user_code).toMatch(/^[A-Z2-9]{4}-[A-Z2-9]{4}$/);
    expect(await s.pollDevice(d.device_code)).toEqual({ status: "pending" });
    expect(await s.deviceLabel(d.user_code.toLowerCase().replace("-", ""))).toBe("my laptop");
    expect(await s.approveDevice(d.user_code, u.id)).toBe(true);
    expect(await s.approveDevice(d.user_code, "attacker")).toBe(false);
    const r = await s.pollDevice(d.device_code);
    expect(r.status).toBe("ok");
    if (r.status === "ok") expect((await s.userForToken(r.key))?.id).toBe(u.id);
    expect(await s.pollDevice(d.device_code)).toEqual({ status: "expired" });
  });
});

test("isValidDid / normalizeUserCode", () => {
  expect(isValidDid(DID)).toBe(true);
  expect(isValidDid("did:web:c0mpute.com")).toBe(true);
  expect(isValidDid("did:")).toBe(false);
  expect(isValidDid(null)).toBe(false);
  expect(normalizeUserCode("abcd efgh")).toBe("ABCD-EFGH");
  expect(normalizeUserCode("abc")).toBe("");
});

describe("parseChatRequest", () => {
  test("validates and strips", () => {
    expect(parseChatRequest({})).toBe("messages[] is required");
    expect(parseChatRequest({ messages: [{ role: "tool", content: "x" }] })).toMatch(/role/);
    const r = parseChatRequest({
      model: "auto",
      messages: [{ role: "user", content: "hi", extra: 1 }],
      temperature: 9,
      max_tokens: 1e9,
      p2p_only: false,
      stream: true,
    });
    expect(r).toEqual({ messages: [{ role: "user", content: "hi" }], stream: true, temperature: 2, max_tokens: 8192 });
  });
});

describe("mcp", () => {
  const user = { id: "u", did: DID, name: null };
  const calls: { path: string; body: unknown }[] = [];
  const upstream: Upstream = async (path, init) => {
    calls.push({ path, body: init?.body ? JSON.parse(String(init.body)) : null });
    if (path === "/v1/models") return Response.json({ data: [{ id: "qwen2.5:7b" }] });
    return Response.json({ model: "qwen2.5:7b", choices: [{ message: { content: "4" } }] });
  };

  test("initialize + tools/list", async () => {
    const init = (await handleRpc({ jsonrpc: "2.0", id: 1, method: "initialize" }, { user, upstream })) as {
      result: { serverInfo: { name: string } };
    };
    expect(init.result.serverInfo.name).toBe("c0mpute-chat");
    const list = (await handleRpc({ id: 2, method: "tools/list" }, { user, upstream })) as {
      result: { tools: { name: string }[] };
    };
    expect(list.result.tools.map((t) => t.name)).toEqual(["chat", "list_models", "whoami"]);
    expect(await handleRpc({ method: "notifications/initialized" }, { user, upstream })).toBeNull();
  });

  test("chat goes to Infernet p2p_only with the system prompt first", async () => {
    const r = (await handleRpc(
      { id: 3, method: "tools/call", params: { name: "chat", arguments: { prompt: "2+2?", system: "terse" } } },
      { user, upstream },
    )) as { result: { content: { text: string }[] } };
    expect(r.result.content[0].text).toBe("4");
    const sent = calls.find((c) => c.path === "/v1/chat/completions")!.body as Record<string, unknown>;
    expect(sent.p2p_only).toBe(true);
    expect(sent.stream).toBe(false);
    expect(sent.messages).toEqual([
      { role: "system", content: "terse" },
      { role: "user", content: "2+2?" },
    ]);
  });
});
