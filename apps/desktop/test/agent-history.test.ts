import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AgentSession, type ChatMessage } from "../src/main/agent";
import { historyEvents, loadAgentHistory, MAX_SAVED, saveAgentHistory } from "../src/main/agent-history";

const tmp = () => join(mkdtempSync(join(tmpdir(), "c0agent-")), "nested", "desktop-agent-chat.json");

describe("agent history on disk", () => {
  test("round trip, 0600, junk dropped, capped", () => {
    const p = tmp();
    expect(loadAgentHistory(p)).toEqual([]);
    const msgs: ChatMessage[] = [
      { role: "user", content: "list my files" },
      { role: "assistant", content: "sure" },
    ];
    saveAgentHistory(msgs, p);
    expect(loadAgentHistory(p)).toEqual(msgs);
    if (process.platform !== "win32") expect(statSync(p).mode & 0o777).toBe(0o600);
    writeFileSync(p, JSON.stringify({ messages: [{ role: "user", content: "ok" }, { role: "tool", content: "x" }, { role: "user" }, null, { role: "assistant", content: "fine", extra: 1 }] }));
    expect(loadAgentHistory(p)).toEqual([
      { role: "user", content: "ok" },
      { role: "assistant", content: "fine" },
    ]);
    writeFileSync(p, "{broken");
    expect(loadAgentHistory(p)).toEqual([]);
    saveAgentHistory(Array.from({ length: MAX_SAVED + 5 }, (_, i) => ({ role: "user" as const, content: String(i) })), p);
    const back = loadAgentHistory(p);
    expect(back.length).toBe(MAX_SAVED);
    expect(back[0].content).toBe("5");
    expect(JSON.parse(readFileSync(p, "utf8")).v).toBe(1);
  });

  test("the session saves after every run and on reset, and a restored one keeps its context", async () => {
    const saved: ChatMessage[][] = [];
    const seen: ChatMessage[][] = [];
    const deps = {
      complete: async (messages: ChatMessage[]) => {
        seen.push(messages);
        return { content: "answer" };
      },
      tools: async () => [],
      systemExtra: () => "",
      confirm: async () => "deny" as const,
      emit: () => {},
      persist: (m: ChatMessage[]) => saved.push(m.map((x) => ({ ...x }))),
    };
    const a = new AgentSession(deps);
    await a.submit("hello");
    expect(saved.at(-1)).toEqual([
      { role: "user", content: "hello" },
      { role: "assistant", content: "answer" },
    ]);
    // A new app start: messages come back from disk and the model sees them.
    const b = new AgentSession(deps);
    b.messages = saved.at(-1)!.map((x) => ({ ...x }));
    await b.submit("and again");
    expect(seen.at(-1)!.slice(1).map((m) => m.content)).toEqual(["hello", "answer", "and again"]);
    b.reset();
    expect(saved.at(-1)).toEqual([]);
  });

  test("history renders turns and compact tool rows, not the raw protocol text", () => {
    const ev = historyEvents([
      { role: "system", content: "sys" },
      { role: "user", content: "read notes.md" },
      { role: "assistant", content: 'Reading it.\n<tool_call>{"name":"fs__read","arguments":{"path":"notes.md"}}</tool_call>' },
      { role: "user", content: '<tool_result name="fs__read" id="c1">\nhello notes\n</tool_result>\n\n<tool_result name="boom" id="c2" error="true">\nnope\n</tool_result>' },
      { role: "assistant", content: "It says hello notes." },
    ]);
    expect(ev.map((e) => e.type)).toEqual(["user", "assistant", "tool_call", "tool_result", "tool_result", "assistant"]);
    expect(ev[1]).toEqual({ type: "assistant", text: "Reading it." });
    expect(ev[2]).toMatchObject({ type: "tool_call", name: "fs__read", arguments: { path: "notes.md" } });
    expect(ev[3]).toEqual({ type: "tool_result", id: "c1", name: "fs__read", output: "hello notes", error: false });
    expect(ev[4]).toMatchObject({ id: "c2", name: "boom", error: true });
  });
});
