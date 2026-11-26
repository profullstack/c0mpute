import { beforeEach, describe, expect, test } from "bun:test";
import { captureWorker, conversationKey, preferredWorker, rememberWorker, resetAffinity } from "./affinity";
import { forwardChat, type ChatMessage, type Upstream } from "./completions";

const W1 = "12D3KooWAbCdEfGhJkLmNpQrStUvWxYz123456789aBcDeFgHi";
const W2 = "12D3KooWZyXwVuTsRqPnMkJhGfEdCbA987654321zYxWvUtSrQ";
const u = (content: string): ChatMessage => ({ role: "user", content });
const a = (content: string): ChatMessage => ({ role: "assistant", content });

beforeEach(() => resetAffinity());

describe("conversationKey", () => {
  test("same first user message and model = same conversation, whatever follows", () => {
    const k1 = conversationKey("m", [u("plan a trip")]);
    const k2 = conversationKey("m", [{ role: "system", content: "s" }, u("plan a trip"), a("ok"), u("to Lisbon")]);
    expect(k1).toBe(k2);
    expect(conversationKey("other", [u("plan a trip")])).not.toBe(k1);
    expect(conversationKey("m", [u("something else")])).not.toBe(k1);
    expect(conversationKey("m", [a("no user")])).toBeNull();
  });
});

describe("remember / prefer", () => {
  test("remembers valid peer ids only, and forgets after an hour", () => {
    rememberWorker("k", "not a peer");
    expect(preferredWorker("k")).toBeNull();
    rememberWorker("k", W1, 1_000);
    expect(preferredWorker("k", 2_000)).toBe(W1);
    expect(preferredWorker("k", 1_000 + 61 * 60_000)).toBeNull();
  });
});

describe("captureWorker", () => {
  test("JSON answers: the x-infernet-worker header", () => {
    const res = new Response("{}", { headers: { "content-type": "application/json", "x-infernet-worker": W1 } });
    expect(captureWorker(res, "k")).toBe(res);
    expect(preferredWorker("k")).toBe(W1);
  });

  test("streams: the `: worker` SSE comment, split across chunks, bytes unchanged", async () => {
    const parts = ['data: {"choices":[{"delta":{"content":"hi"}}]}\n\n: wor', `ker {"worker":"${W2}"}\n\n`, "data: [DONE]\n\n"];
    const enc = new TextEncoder();
    const body = new ReadableStream<Uint8Array>({
      start(c) {
        for (const p of parts) c.enqueue(enc.encode(p));
        c.close();
      },
    });
    const res = captureWorker(new Response(body, { headers: { "content-type": "text/event-stream" } }), "k");
    expect(await res.text()).toBe(parts.join(""));
    expect(preferredWorker("k")).toBe(W2);
  });
});

describe("forwardChat", () => {
  test("turn 2 of a conversation asks for the worker that answered turn 1", async () => {
    const sent: Record<string, unknown>[] = [];
    const upstream: Upstream = async (_path, init) => {
      sent.push(JSON.parse(String(init?.body)));
      return new Response(JSON.stringify({ choices: [{ message: { content: "ok" } }] }), {
        headers: { "content-type": "application/json", "x-infernet-worker": W1 },
      });
    };
    await forwardChat(upstream, { model: "m", messages: [u("hello there")] });
    await forwardChat(upstream, { model: "m", messages: [u("hello there"), a("ok"), u("and then?")] });
    await forwardChat(upstream, { model: "m", messages: [u("a different chat")] });
    expect(sent[0].prefer_worker).toBeUndefined();
    expect(sent[1].prefer_worker).toBe(W1);
    expect(sent[2].prefer_worker).toBeUndefined();
  });

  test("an error answer is passed back as-is and remembers nothing", async () => {
    const upstream: Upstream = async () => new Response("{}", { status: 503, headers: { "x-infernet-worker": W1 } });
    const res = await forwardChat(upstream, { model: "m", messages: [u("x")] });
    expect(res.status).toBe(503);
    expect(preferredWorker(conversationKey("m", [u("x")]))).toBeNull();
  });
});
