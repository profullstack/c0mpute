// A real MCP round trip: the app's client spawns a stdio server (the
// echo-server fixture, run with bun), lists its tools and calls them, with a
// ${VAR} env placeholder filled from the lookup at connect time.
import { afterAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AgentSession, type AgentEvent } from "../src/main/agent";
import { buildTools } from "../src/main/agent-tools";
import { McpManager, qualifiedName } from "../src/main/mcp-client";
import { loadMcpDir } from "../src/main/mcp-config";
import { DEFAULT_SETTINGS } from "../src/main/settings";
import { loadSkills } from "../src/main/skills";

const dir = mkdtempSync(join(tmpdir(), "c0-mcp-rt-"));
writeFileSync(
  join(dir, "echo.json"),
  JSON.stringify({
    _note: "fixture",
    mcpServers: {
      echo: { command: process.execPath, args: [join(import.meta.dir, "fixtures", "echo-server.ts")], env: { FIXTURE_TOKEN: "${FIXTURE_TOKEN}" } },
    },
  }),
);
const mgr = new McpManager((name) => (name === "FIXTURE_TOKEN" ? "tok-123456" : undefined));
afterAll(async () => {
  await mgr.closeAll();
  rmSync(dir, { recursive: true, force: true });
});

describe("MCP stdio round trip", () => {
  test("connect, list tools, call", async () => {
    const { servers, errors } = loadMcpDir(dir);
    expect(errors).toEqual([]);
    await mgr.sync(servers);
    const conn = mgr.conns.get("echo")!;
    expect(conn.error).toBeUndefined();
    expect(conn.state).toBe("ready");
    expect(conn.tools.map((t) => t.name).sort()).toEqual(["add", "echo", "secret"]);
    expect(await mgr.call("echo", "echo", { text: "hello" })).toEqual({ text: "echo: hello", isError: false });
    expect((await mgr.call("echo", "add", { a: 2, b: 40 })).text).toBe("42");
    // The placeholder was expanded for the child only.
    expect((await mgr.call("echo", "secret", {})).text).toBe("token length 10");
    expect(servers[0].env!.FIXTURE_TOKEN).toBe("${FIXTURE_TOKEN}");
  }, 60_000);

  test("the agent calls an MCP tool end to end (fake model, real server)", async () => {
    const events: AgentEvent[] = [];
    const replies = [`<tool_call>{"name":"${qualifiedName("echo", "add")}","arguments":{"a":20,"b":22}}</tool_call>`, "The answer is 42."];
    const agent = new AgentSession({
      complete: async () => ({ content: replies.shift() ?? "" }),
      tools: async () => buildTools({ mcp: mgr, skills: loadSkills(join(import.meta.dir, "fixtures", "skills")), settings: { ...DEFAULT_SETTINGS, mcpAllowed: ["echo"] } }),
      systemExtra: () => "",
      confirm: async () => "deny",
      emit: (e) => events.push(e),
    });
    await agent.send("what is 20 + 22?");
    const result = events.find((e) => e.type === "tool_result");
    expect(result).toMatchObject({ name: "echo__add", output: "42", error: false });
    expect(events.find((e) => e.type === "assistant")).toMatchObject({ text: "The answer is 42." });
  }, 60_000);

  test("a missing ${VAR} is a clear error, not a crash", async () => {
    const m2 = new McpManager(() => undefined);
    const c = await m2.connect(loadMcpDir(dir).servers[0]);
    expect(c.state).toBe("error");
    expect(c.error).toContain("FIXTURE_TOKEN");
    await m2.closeAll();
  });
});
