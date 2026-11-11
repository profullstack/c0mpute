import { beforeEach, describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { ConvoError, convoKey, createConvoStore, parseConversation } from "./convo-store";
import { _resetRuns, cancelRun, getRun, listRuns, runStream, startRun } from "./runs";
import type { Sql } from "./store";
import { adopt, slimForSync, syncPlan } from "./sync-merge";
import { handleRpc } from "./mcp";
import type { Upstream } from "./completions";

function memSql(): { sql: Sql; db: Database } {
  const db = new Database(":memory:");
  return {
    db,
    sql: {
      all: async (q, p = []) => db.query(q).all(...(p as never[])) as Record<string, unknown>[],
      run: async (q, p = []) => void db.query(q).run(...(p as never[])),
    },
  };
}

const KEY = Buffer.alloc(32, 7);
const convo = (updated: number, content = "hi") => parseConversation("abcd1234", { title: "t", model: "auto", updated, messages: [{ role: "user", content }, { role: "note", content: "local", jobId: "j1" }] });

describe("conversation store", () => {
  test("round trip, sealed at rest, bound to its owner", async () => {
    const { sql, db } = memSql();
    const s = createConvoStore(sql, { key: KEY });
    await s.put("u1", convo(100, "secret plans"));
    const back = await s.get("u1", "abcd1234");
    expect(back?.messages).toEqual([{ role: "user", content: "secret plans" }, { role: "note", content: "local", jobId: "j1" }]);
    const raw = db.query("SELECT messages FROM chat_conversations").get() as { messages: string };
    expect(raw.messages).not.toContain("secret");
    expect(await s.get("u2", "abcd1234")).toBeNull();
    // A row copied to another account cannot be opened there (AAD binds it).
    db.query("UPDATE chat_conversations SET user_id = 'u2'").run();
    expect(await s.get("u2", "abcd1234")).toBeNull();
  });

  test("last writer wins; an older copy gets 409 with the newer one", async () => {
    const s = createConvoStore(memSql().sql, { key: KEY });
    await s.put("u", convo(200, "newer"));
    let err: unknown;
    await s.put("u", convo(100, "older")).catch((e) => (err = e));
    expect(err).toBeInstanceOf(ConvoError);
    expect((err as ConvoError).status).toBe(409);
    expect((err as ConvoError).conversation?.messages[0].content).toBe("newer");
    await s.put("u", convo(300, "newest"));
    expect((await s.get("u", "abcd1234"))?.messages[0].content).toBe("newest");
  });

  test("delete leaves a tombstone that incremental sync reports", async () => {
    const s = createConvoStore(memSql().sql, { key: KEY });
    await s.put("u", convo(100));
    expect(await s.remove("u", "abcd1234", 150)).toBe(true);
    expect(await s.get("u", "abcd1234")).toBeNull();
    expect(await s.list("u")).toEqual([]);
    expect(await s.list("u", 120)).toEqual([{ id: "abcd1234", title: "t", model: "auto", updated: 150, deleted: true }]);
    expect(await s.remove("u", "abcd1234")).toBe(false);
    // Recreated with a newer clock, it comes back.
    await s.put("u", convo(200));
    expect((await s.list("u")).length).toBe(1);
  });

  test("validation and limits", () => {
    expect(() => parseConversation("x", { messages: [], updated: 1 })).toThrow(/4-64/);
    expect(() => parseConversation("abcd", { messages: [{ role: "tool", content: "" }], updated: 1 })).toThrow(/role/);
    expect(() => parseConversation("abcd", { messages: [], updated: 0 })).toThrow(/updated/);
    expect(parseConversation("abcd", { messages: [], updated: 5 })).toEqual({ id: "abcd", title: "new chat", model: "auto", messages: [], updated: 5 });
  });

  test("too big is refused; no key means sync is off, not broken", async () => {
    const s = createConvoStore(memSql().sql, { key: KEY });
    const big = parseConversation("abcd", { messages: [{ role: "user", content: "x".repeat(1024 * 1024 + 1) }], updated: 1 });
    expect(s.put("u", big)).rejects.toThrow(/at most/);
    const off = createConvoStore(memSql().sql, { key: null });
    let status = 0;
    await off.put("u", convo(1)).catch((e: ConvoError) => (status = e.status));
    expect(status).toBe(503);
  });

  test("the key comes from C0MPUTE_CHAT_KEY or is derived from the plugin key", () => {
    const own = Buffer.alloc(32, 1);
    expect(convoKey(own, KEY)).toBe(own);
    const derived = convoKey(null, KEY)!;
    expect(derived.length).toBe(32);
    expect(derived.equals(KEY)).toBe(false);
    expect(convoKey(null, null)).toBeNull();
  });
});

const enc = new TextEncoder();
const text = async (s: ReadableStream<Uint8Array>) => new Response(s).text();

describe("resumable runs", () => {
  beforeEach(() => _resetRuns());

  test("a turn keeps running with no reader; a late reader replays it all then follows live", async () => {
    let push: (s: string) => void = () => {};
    let close: () => void = () => {};
    const run = await startRun("u", async () =>
      new ReadableStream<Uint8Array>({
        start(c) {
          push = (s) => c.enqueue(enc.encode(s));
          close = () => c.close();
        },
      }),
    );
    push('data: {"choices":[{"delta":{"content":"Hel"}}]}\n\n');
    await new Promise((r) => setTimeout(r, 5));
    // First reader drops at once (a closed tab).
    const first = runStream(run).getReader();
    await first.read();
    await first.cancel();
    const late = text(runStream(run));
    push('data: {"choices":[{"delta":{"content":"lo"}}]}\n\ndata: [DONE]\n\n');
    close();
    const body = await late;
    expect(body.startsWith(`data: {"object":"c0mpute.run","id":"${run.id}"}`)).toBe(true);
    expect(body).toContain('"Hel"');
    expect(body).toContain('"lo"');
    expect(body.trim().endsWith("[DONE]")).toBe(true);
    expect(run.status).toBe("done");
    // Replay after the end, and from a byte offset.
    expect(await text(runStream(run))).toBe(body);
    expect(await text(runStream(run, 10))).toBe(body.slice(10));
  });

  test("only the owner sees a run; cancel aborts the upstream", async () => {
    let aborted = false;
    const run = await startRun("u", async (signal) => {
      signal.addEventListener("abort", () => (aborted = true));
      return new ReadableStream<Uint8Array>({ start() {} });
    });
    expect(getRun("other", run.id)).toBeNull();
    expect(cancelRun("other", run.id)).toBe(false);
    expect(listRuns("u").map((r) => r.status)).toEqual(["running"]);
    expect(cancelRun("u", run.id)).toBe(true);
    expect(aborted).toBe(true);
    expect(run.status).toBe("cancelled");
    expect((await text(runStream(run))).trim().endsWith("[DONE]")).toBe(true);
  });

  test("an upstream that fails to open ends the run with an error a client can show", async () => {
    const run = await startRun("u", async () => {
      throw new Error("no provider node");
    });
    expect(run.status).toBe("failed");
    expect(await text(runStream(run))).toContain("no provider node");
  });

  test("too many running turns on one account is refused", async () => {
    for (let i = 0; i < 6; i++) await startRun("busy", async () => new ReadableStream<Uint8Array>({ start() {} }));
    expect(startRun("busy", async () => null)).rejects.toThrow(/at most 6/);
    await startRun("someone-else", async () => new ReadableStream<Uint8Array>({ start() {} }));
  });
});

describe("sync merge (client)", () => {
  test("plan: pull newer or missing, push newer or unknown, drop deleted unless changed after", () => {
    const local = [
      { id: "same", updated: 5 },
      { id: "mine-newer", updated: 9 },
      { id: "theirs-newer", updated: 1 },
      { id: "only-here", updated: 3 },
      { id: "deleted-there", updated: 2 },
      { id: "edited-after-delete", updated: 20 },
    ];
    const s = (id: string, updated: number, deleted = false) => ({ id, title: id, model: "auto", updated, deleted });
    const plan = syncPlan(local, [s("same", 5), s("mine-newer", 4), s("theirs-newer", 8), s("only-there", 7), s("deleted-there", 10, true), s("edited-after-delete", 10, true)]);
    expect(plan.pull.sort()).toEqual(["only-there", "theirs-newer"]);
    expect(plan.push.sort()).toEqual(["edited-after-delete", "mine-newer", "only-here"]);
    expect(plan.drop).toEqual(["deleted-there"]);
  });

  test("slim drops media, data: URLs and the live run id; adopt keeps this device's media", () => {
    const local = {
      id: "abcd",
      title: "t",
      model: "auto",
      updated: 1,
      messages: [
        { role: "user", content: "/image a cat" },
        { role: "assistant", content: "// image", image: "data:image/png;base64,AAA", imageJob: "job1", run: "run_x" },
        { role: "weird", content: "x" },
      ],
    };
    const slim = slimForSync(local);
    expect(slim.messages).toEqual([
      { role: "user", content: "/image a cat" },
      { role: "assistant", content: "// image", imageJob: "job1" },
    ]);
    const back = adopt(local, { ...slim, updated: 2, messages: [...slim.messages, { role: "user", content: "more" }] });
    expect(back.messages[1].image).toBe("data:image/png;base64,AAA");
    expect(back.messages[2]).toEqual({ role: "user", content: "more" });
    expect(back.updated).toBe(2);
  });
});

describe("MCP conversations", () => {
  test("chat continues a synced conversation and saves the turn; the tools list and read it", async () => {
    const convos = createConvoStore(memSql().sql, { key: KEY });
    await convos.put("u", parseConversation("web123", { title: "from the browser", model: "auto", updated: 10, messages: [{ role: "user", content: "my name is Ada" }, { role: "assistant", content: "hi Ada" }, { role: "note", content: "local remark" }] }));
    let sent: { role: string; content: string }[] = [];
    const upstream: Upstream = async (_p, init) => {
      sent = JSON.parse(String(init?.body)).messages;
      return Response.json({ model: "m", choices: [{ message: { content: "Your name is Ada." } }] });
    };
    const user = { id: "u", did: "did:key:z", name: null };
    const r = (await handleRpc({ id: 1, method: "tools/call", params: { name: "chat", arguments: { prompt: "what is my name?", conversation: "web123" } } }, { user, upstream, convos })) as { result: { content: { text: string }[] } };
    expect(r.result.content[0].text).toBe("Your name is Ada.");
    // History first (notes never go to the model), then the new prompt.
    expect(sent.filter((m) => m.role !== "system")).toEqual([
      { role: "user", content: "my name is Ada" },
      { role: "assistant", content: "hi Ada" },
      { role: "user", content: "what is my name?" },
    ]);
    const saved = await convos.get("u", "web123");
    expect(saved?.messages.slice(-2)).toEqual([
      { role: "user", content: "what is my name?" },
      { role: "assistant", content: "Your name is Ada." },
    ]);
    expect(saved!.updated).toBeGreaterThan(10);

    const list = (await handleRpc({ id: 2, method: "tools/call", params: { name: "chat_conversations", arguments: {} } }, { user, upstream, convos })) as { result: { content: { text: string }[] } };
    expect(list.result.content[0].text).toContain("web123");
    const read = (await handleRpc({ id: 3, method: "tools/call", params: { name: "chat_conversation", arguments: { id: "web123" } } }, { user, upstream, convos })) as { result: { content: { text: string }[] } };
    expect(read.result.content[0].text).toContain("assistant: Your name is Ada.");
    const tools = (await handleRpc({ id: 4, method: "tools/list" }, { user, upstream, convos })) as { result: { tools: { name: string }[] } };
    expect(tools.result.tools.map((t) => t.name)).toContain("chat_conversations");
    // A new id starts a conversation.
    await handleRpc({ id: 5, method: "tools/call", params: { name: "chat", arguments: { prompt: "start fresh", conversation: "mcp-new-1" } } }, { user, upstream, convos });
    expect((await convos.get("u", "mcp-new-1"))?.title).toBe("start fresh");
  });
});
