import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { createStore, isValidDid, NoDidError, normalizeUserCode, type Sql } from "./store";
import { forwardChat, parseChatRequest, withHousePrompt, C0_SYSTEM_PROMPT, type Upstream } from "./completions";
import { handleRpc, inflightCalls } from "./mcp";
import { withSignal } from "./plugins";

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

  test("adult mode: off by default, attested with a time, cleared on opt-out", async () => {
    const sql = memSql();
    const s = createStore(sql);
    const u = await s.upsertCoinpayUser({ sub: "adult-1", did: DID });
    const session = await s.createSession(u.id);
    expect((await s.userForToken(session))?.adult_verified_at ?? null).toBeNull();
    const at = await s.setAdultVerified(u.id, true);
    expect(at).toMatch(/^\d{4}-\d{2}-\d{2}T/);
    expect((await s.userForToken(session))?.adult_verified_at).toBe(at);
    await s.setAdultVerified(u.id, false);
    expect((await s.userForToken(session))?.adult_verified_at ?? null).toBeNull();
    // The migration is idempotent: a second store on the same database starts cleanly.
    await createStore(sql).init();
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

describe("house system prompt", () => {
  test("prepends c0 unless the caller sent a system message", async () => {
    const user = { role: "user" as const, content: "who is this?" };
    expect(withHousePrompt([user])).toEqual([{ role: "system", content: C0_SYSTEM_PROMPT }, user]);
    const own = [{ role: "system" as const, content: "be terse" }, user];
    expect(withHousePrompt(own)).toEqual(own);
    const stub = { role: "assistant" as const, content: 'Running on the Infernet P2P network. You said: "hi" This is a stub response from the provider daemon — no real model.' };
    const err = { role: "assistant" as const, content: "ollama HTTP 404: {\"error\":\"model 'qwen2.5:0.5b' not found\"}" };
    const real = { role: "assistant" as const, content: "Hello! How can I help?" };
    expect(withHousePrompt([user, stub, user, err, user, real, user]).slice(1)).toEqual([user, user, user, real, user]);
    let sent: Record<string, unknown> = {};
    const up: Upstream = async (_p, init) => {
      sent = JSON.parse(String(init?.body));
      return new Response("{}");
    };
    await forwardChat(up, { messages: [user] });
    expect((sent.messages as { role: string }[])[0].role).toBe("system");
    expect(sent.p2p_only).toBe(true);
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
    expect(init.result.serverInfo.name).toBe("c0mpute");
    const list = (await handleRpc({ id: 2, method: "tools/list" }, { user, upstream })) as {
      result: { tools: { name: string }[] };
    };
    expect(list.result.tools.map((t) => t.name)).toEqual(["chat", "list_models", "promo_video", "promo_status", "whoami"]);
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

describe("interrupting a chat", () => {
  const user = { id: "u-cancel", did: DID, name: null };

  test("notifications/cancelled aborts the running MCP chat call", async () => {
    let upstreamSignal: AbortSignal | undefined;
    const upstream: Upstream = (_path, init) => {
      upstreamSignal = init?.signal ?? undefined;
      return new Promise<Response>((_, reject) => init?.signal?.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError"))));
    };
    const pending = handleRpc({ id: 41, method: "tools/call", params: { name: "chat", arguments: { prompt: "a long answer" } } }, { user, upstream });
    await new Promise((r) => setTimeout(r, 5));
    expect(inflightCalls()).toBe(1);
    // Another user's cancel for the same id does nothing.
    expect(await handleRpc({ method: "notifications/cancelled", params: { requestId: 41 } }, { user: { ...user, id: "other" }, upstream })).toBeNull();
    expect(upstreamSignal?.aborted).toBe(false);
    expect(await handleRpc({ method: "notifications/cancelled", params: { requestId: 41, reason: "user pressed Esc" } }, { user, upstream })).toBeNull();
    const r = (await pending) as { result: { content: { text: string }[]; isError?: boolean } };
    expect(r.result.content[0].text).toBe("cancelled");
    expect(r.result.isError).toBe(true);
    expect(upstreamSignal?.aborted).toBe(true);
    expect(inflightCalls()).toBe(0);
  });

  test("the HTTP request closing aborts the call too", async () => {
    const upstream: Upstream = (_path, init) =>
      new Promise<Response>((_, reject) => init?.signal?.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError"))));
    const hangup = new AbortController();
    const pending = handleRpc({ id: 42, method: "tools/call", params: { name: "chat", arguments: { prompt: "hi" } } }, { user, upstream, signal: hangup.signal });
    await new Promise((r) => setTimeout(r, 5));
    hangup.abort();
    const r = (await pending) as { result: { isError?: boolean; content: { text: string }[] } };
    expect(r.result.isError).toBe(true);
    expect(inflightCalls()).toBe(0);
  });

  test("withSignal threads Stop into plugin tool requests", async () => {
    const seen: (AbortSignal | null | undefined)[] = [];
    const base = (async (_input: unknown, init?: RequestInit) => {
      seen.push(init?.signal);
      return new Response("{}");
    }) as unknown as typeof fetch;
    const stop = new AbortController();
    const net = withSignal({ fetch: base, timeoutMs: 1000 }, stop.signal);
    expect(net.timeoutMs).toBe(1000);
    const own = new AbortController();
    await net.fetch!("https://x.test/mcp", { signal: own.signal });
    await net.fetch!("https://x.test/mcp");
    stop.abort();
    expect(seen[0]?.aborted).toBe(true);
    expect(seen[1]?.aborted).toBe(true);
    expect(withSignal({ timeoutMs: 5 })).toEqual({ timeoutMs: 5 });
  });
});
