import { describe, expect, test } from "bun:test";
import { streamChatText, type Upstream } from "./completions";

const enc = new TextEncoder();

/** An SSE upstream that sends `parts` with `gapMs` between them. */
function sseUpstream(parts: string[], gapMs = 0): Upstream {
  return async (_path, init) => {
    const signal = init?.signal;
    const body = new ReadableStream<Uint8Array>({
      async start(ctrl) {
        for (const p of parts) {
          if (gapMs) await new Promise((r) => setTimeout(r, gapMs));
          if (signal?.aborted) return ctrl.error(signal.reason);
          ctrl.enqueue(enc.encode(p));
        }
        ctrl.close();
      },
    });
    return new Response(body, { headers: { "content-type": "text/event-stream" } });
  };
}

const chunk = (content: string) => `data: ${JSON.stringify({ model: "m-9b", choices: [{ delta: { content } }] })}\n\n`;

describe("streamChatText", () => {
  test("collects the text and passes the network's status lines through", async () => {
    const seen: string[] = [];
    const up = sseUpstream([`: status ${JSON.stringify({ message: "queued: 1 ahead" })}\n\n`, chunk("Pass"), chunk("keys "), chunk("win."), "data: [DONE]\n\n"]);
    const r = await streamChatText(up, { messages: [{ role: "user", content: "2FA vs passkeys?" }] }, { onStatus: (m) => seen.push(m) });
    expect(r).toEqual({ text: "Passkeys win.", model: "m-9b" });
    expect(seen[0]).toBe("queued: 1 ahead");
    expect(seen.some((s) => s.startsWith("a node is writing"))).toBe(true);
  });

  test("a slow node that keeps writing is not cut off (no total ceiling)", async () => {
    const parts = Array.from({ length: 8 }, (_, i) => chunk(`${i}`));
    const r = await streamChatText(sseUpstream([...parts, "data: [DONE]\n\n"], 20), { messages: [{ role: "user", content: "x" }] }, { idleMs: 60, firstByteMs: 60 });
    expect(r.text).toBe("01234567");
  });

  test("a node that goes quiet fails with a clear idle message", async () => {
    const up = sseUpstream([chunk("a"), chunk("b")], 120);
    await expect(streamChatText(up, { messages: [{ role: "user", content: "x" }] }, { idleMs: 50, firstByteMs: 200 })).rejects.toThrow(/stopped answering/);
  });

  test("an upstream error chunk becomes the error", async () => {
    const up = sseUpstream([`data: ${JSON.stringify({ error: { message: "no provider" } })}\n\n`]);
    await expect(streamChatText(up, { messages: [{ role: "user", content: "x" }] })).rejects.toThrow("no provider");
  });

  test("a JSON answer (no SSE) still works", async () => {
    const up: Upstream = async () => Response.json({ model: "j", choices: [{ message: { content: "hi" } }] });
    expect(await streamChatText(up, { messages: [{ role: "user", content: "x" }] })).toEqual({ text: "hi", model: "j" });
  });
});
