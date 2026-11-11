import { describe, expect, test } from "bun:test";
import { AgentSession, type AgentEvent, type AgentTool, type ChatMessage, type Decision } from "../src/main/agent";
import { extractToolCalls, nativeToolCalls, toolsPrompt } from "../src/main/tool-protocol";

describe("tool call parsing", () => {
  test("tagged JSON calls, several per reply, text kept", () => {
    const r = extractToolCalls('Let me check.\n<tool_call>{"name": "a__b", "arguments": {"x": 1}}</tool_call>\n<tool_call>{"name":"c","arguments":"{\\"y\\":2}"}</tool_call>');
    expect(r.text).toBe("Let me check.");
    expect(r.calls.map((c) => [c.name, c.arguments])).toEqual([
      ["a__b", { x: 1 }],
      ["c", { y: 2 }],
    ]);
  });

  test("unclosed final tag, code fence, think blocks", () => {
    const r = extractToolCalls('<think>hmm</think>ok <tool_call>```json\n{"name":"t","arguments":{}}\n```');
    expect(r.text).toBe("ok");
    expect(r.calls[0].name).toBe("t");
  });

  test("garbage inside a tag becomes an error call", () => {
    expect(extractToolCalls("<tool_call>not json</tool_call>").calls[0].error).toContain("could not parse");
  });

  test("OpenAI tool_calls", () => {
    const calls = nativeToolCalls({ tool_calls: [{ id: "c1", function: { name: "f", arguments: '{"a":1}' } }] });
    expect(calls).toEqual([{ id: "c1", name: "f", arguments: { a: 1 } }]);
  });

  test("prompt lists tools with their schema", () => {
    const p = toolsPrompt([{ name: "x", description: "d", parameters: { type: "object" } }]);
    expect(p).toContain("<tool_call>");
    expect(p).toContain('"name":"x"');
  });
});

function harness(replies: string[], decisions: Decision[] = []) {
  const events: AgentEvent[] = [];
  const seen: ChatMessage[][] = [];
  const ran: unknown[] = [];
  const tool = (name: string, source: string, confirm: boolean): AgentTool => ({
    name,
    description: name,
    parameters: { type: "object" },
    source,
    confirm,
    run: async (args) => {
      ran.push([name, args]);
      if (name === "boom") throw new Error("tool exploded");
      return `ran ${name} ${JSON.stringify(args)}`;
    },
  });
  const agent = new AgentSession({
    complete: async (messages) => {
      seen.push(messages);
      return { content: replies.shift() ?? "done", model: "fake" };
    },
    tools: async () => [tool("srv__echo", "mcp:srv", true), tool("load_skill", "skills", false), tool("boom", "mcp:srv", false), tool("shell", "shell", true)],
    systemExtra: () => "## Skills\n- hello-world: says hi",
    confirm: async () => decisions.shift() ?? "deny",
    emit: (e) => events.push(e),
  });
  return { agent, events, seen, ran };
}

describe("agent loop", () => {
  test("runs a tool, feeds the result back, ends on a plain answer", async () => {
    const h = harness(['<tool_call>{"name":"load_skill","arguments":{"name":"hello-world"}}</tool_call>', "Hello!"]);
    await h.agent.send("say hi");
    expect(h.ran).toEqual([["load_skill", { name: "hello-world" }]]);
    expect(h.seen[0][0].role).toBe("system");
    expect(h.seen[0][0].content).toContain("hello-world: says hi");
    expect(h.seen[0][0].content).toContain('"name":"srv__echo"');
    const second = h.seen[1];
    expect(second.at(-1)!.content).toContain("<tool_result");
    expect(second.at(-1)!.content).toContain("ran load_skill");
    expect(h.events.map((e) => e.type)).toEqual(["user", "status", "tool_call", "tool_result", "status", "assistant", "done"]);
  });

  test("confirmation: deny is reported to the model, always allows the server for the session", async () => {
    const h = harness(
      [
        '<tool_call>{"name":"srv__echo","arguments":{"t":1}}</tool_call>',
        '<tool_call>{"name":"srv__echo","arguments":{"t":2}}</tool_call>',
        '<tool_call>{"name":"srv__echo","arguments":{"t":3}}</tool_call>',
        "ok",
      ],
      ["deny", "always"],
    );
    await h.agent.send("go");
    expect(h.ran).toEqual([
      ["srv__echo", { t: 2 }],
      ["srv__echo", { t: 3 }],
    ]);
    expect(h.seen[1].at(-1)!.content).toContain("declined");
    expect(h.agent.allowed.has("mcp:srv")).toBe(true);
  });

  test("the shell is confirmed every time, even after 'always'", async () => {
    const h = harness(
      ['<tool_call>{"name":"shell","arguments":{"command":"ls"}}</tool_call>', '<tool_call>{"name":"shell","arguments":{"command":"pwd"}}</tool_call>', "ok"],
      ["always", "deny"],
    );
    await h.agent.send("go");
    expect(h.ran).toEqual([["shell", { command: "ls" }]]);
    expect(h.agent.allowed.has("shell")).toBe(false);
  });

  test("unknown tools and tool errors go back to the model", async () => {
    const h = harness(['<tool_call>{"name":"nope","arguments":{}}</tool_call><tool_call>{"name":"boom","arguments":{}}</tool_call>', "sorry"]);
    await h.agent.send("go");
    const fed = h.seen[1].at(-1)!.content;
    expect(fed).toContain("no tool named");
    expect(fed).toContain("tool exploded");
    expect(fed).toContain('error="true"');
  });

  test("stops after maxSteps", async () => {
    const h = harness(Array(20).fill('<tool_call>{"name":"load_skill","arguments":{}}</tool_call>'));
    h.agent.maxSteps = 3;
    await h.agent.send("loop");
    expect(h.ran.length).toBe(3);
    expect(h.events.some((e) => e.type === "error" && e.text.includes("3 tool steps"))).toBe(true);
  });
});
