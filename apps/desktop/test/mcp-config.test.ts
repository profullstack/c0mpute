import { afterEach, describe, expect, test } from "bun:test";
import { cpSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  deleteServer,
  expandServer,
  importServers,
  isPlaceholderOnly,
  loadMcpDir,
  MASK,
  maskServer,
  parseMcpFile,
  placeholders,
  readClaudeServers,
  saveServer,
  unmask,
  type McpServer,
} from "../src/main/mcp-config";

const FIXTURES = join(import.meta.dir, "fixtures", "mcps");
const dirs: string[] = [];
function scratch() {
  const d = mkdtempSync(join(tmpdir(), "c0-mcps-"));
  dirs.push(d);
  return d;
}
afterEach(() => {
  for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
});

describe("~/mcps loading (real files from ~/mcps as fixtures)", () => {
  test("loads stdio and http servers and ignores _note", () => {
    const r = loadMcpDir(FIXTURES);
    expect(r.errors).toEqual([]);
    const by = Object.fromEntries(r.servers.map((s) => [s.name, s]));
    expect(Object.keys(by).sort()).toEqual(["crawlproof", "media2markdown", "readm3", "typeheard"]);
    expect(by.media2markdown).toMatchObject({ transport: "stdio", command: "npx", args: ["-y", "@profullstack/media2markdown-mcp"], disabled: false });
    expect(by.readm3).toMatchObject({ transport: "http", url: "https://readm3.com/mcp" });
    expect(by.crawlproof.headers).toEqual({ Authorization: "Bearer ${CRAWLPROOF_API_KEY}" });
    expect(by.typeheard.env).toEqual({ TYPEHEARD_API_KEY: "${TYPEHEARD_API_KEY}" });
    expect(by.crawlproof.note).toContain("CrawlProof");
  });

  test("creates the folder on first run", () => {
    const d = join(scratch(), "mcps");
    expect(loadMcpDir(d).servers).toEqual([]);
    expect(existsSync(d)).toBe(true);
  });

  test("many servers per file, bad entries reported, the rest loaded", () => {
    const r = parseMcpFile(
      JSON.stringify({ mcpServers: { a: { command: "x" }, b: { type: "sse", url: "https://b/sse" }, c: { args: [] }, "bad name!": { command: "y" } } }),
      "/f.json",
    );
    expect(r.servers.map((s) => [s.name, s.transport])).toEqual([
      ["a", "stdio"],
      ["b", "sse"],
    ]);
    expect(r.errors.length).toBe(2);
    expect(parseMcpFile("{nope", "/g.json").errors[0].error).toContain("not JSON");
    expect(parseMcpFile("{}", "/g.json").errors[0].error).toContain("mcpServers");
  });

  test("a name defined twice: the first file wins", () => {
    const d = scratch();
    writeFileSync(join(d, "a.json"), JSON.stringify({ mcpServers: { x: { command: "one" } } }));
    writeFileSync(join(d, "b.json"), JSON.stringify({ mcpServers: { x: { command: "two" } } }));
    const r = loadMcpDir(d);
    expect(r.servers.map((s) => s.command)).toEqual(["one"]);
    expect(r.errors[0].error).toContain("already defined");
  });
});

describe("writing back", () => {
  test("edit keeps _note, other servers and unknown keys; placeholders stay placeholders", () => {
    const d = scratch();
    cpSync(join(FIXTURES, "crawlproof.json"), join(d, "crawlproof.json"));
    writeFileSync(
      join(d, "pair.json"),
      JSON.stringify({ _note: "two in one", mcpServers: { one: { command: "a", autoApprove: ["t"] }, two: { command: "b" } } }),
    );
    const [one] = loadMcpDir(d).servers.filter((s) => s.name === "one");
    saveServer(d, { ...one, args: ["--x"] }, one);
    const doc = JSON.parse(readFileSync(join(d, "pair.json"), "utf8"));
    expect(doc._note).toBe("two in one");
    expect(doc.mcpServers.one).toEqual({ command: "a", args: ["--x"], autoApprove: ["t"] });
    expect(doc.mcpServers.two).toEqual({ command: "b" });

    // Disable / enable crawlproof: its ${VAR} header is written back untouched.
    const cp = loadMcpDir(d).servers.find((s) => s.name === "crawlproof")!;
    saveServer(d, { ...cp, disabled: true }, cp);
    const raw = readFileSync(join(d, "crawlproof.json"), "utf8");
    expect(raw).toContain("Bearer ${CRAWLPROOF_API_KEY}");
    expect(JSON.parse(raw)._note).toContain("CrawlProof");
    expect(loadMcpDir(d).servers.find((s) => s.name === "crawlproof")!.disabled).toBe(true);
  });

  test("new server goes to <name>.json; rename and delete", () => {
    const d = scratch();
    const s: McpServer = { name: "local-echo", file: "", transport: "stdio", command: "bun", args: ["echo.ts"], disabled: false };
    saveServer(d, s);
    expect(readdirSync(d)).toEqual(["local-echo.json"]);
    expect(() => saveServer(d, s)).toThrow("already exists");
    const loaded = loadMcpDir(d).servers[0];
    saveServer(d, { ...loaded, name: "echo2" }, loaded);
    expect(Object.keys(JSON.parse(readFileSync(join(d, "local-echo.json"), "utf8")).mcpServers)).toEqual(["echo2"]);
    deleteServer(loadMcpDir(d).servers[0]);
    expect(readdirSync(d)).toEqual([]);
  });
});

describe("secrets", () => {
  const literal: McpServer = {
    name: "x",
    file: "/x.json",
    transport: "http",
    url: "https://x/mcp",
    headers: { Authorization: "Bearer not-a-real-token-value", "X-Trace": "on" },
    env: { API_TOKEN: "abc123", MY_SECRET: "${MY_SECRET}", REGION: "eu" },
    disabled: false,
  };

  test("masks sensitive literal values, leaves placeholders and plain values", () => {
    const m = maskServer(literal);
    expect(m.headers!.Authorization).toBe(`Bearer ${MASK}`);
    expect(m.headers!["X-Trace"]).toBe("on");
    expect(m.env).toEqual({ API_TOKEN: MASK, MY_SECRET: "${MY_SECRET}", REGION: "eu" });
    expect(JSON.stringify(m)).not.toContain("not-a-real-token");
    expect(JSON.stringify(m)).not.toContain("abc123");
    expect(isPlaceholderOnly("Bearer ${CRAWLPROOF_API_KEY}")).toBe(true);
    expect(isPlaceholderOnly("Bearer abc")).toBe(false);
  });

  test("a masked value sent back unchanged keeps the real one", () => {
    const edited = { ...maskServer(literal), env: { ...maskServer(literal).env, REGION: "us" } };
    const back = unmask(edited, literal);
    expect(back.headers!.Authorization).toBe("Bearer not-a-real-token-value");
    expect(back.env).toEqual({ API_TOKEN: "abc123", MY_SECRET: "${MY_SECRET}", REGION: "us" });
  });

  test("expands ${VAR} at connect time and lists what is missing", () => {
    const [cp] = loadMcpDir(FIXTURES).servers.filter((s) => s.name === "crawlproof");
    expect(placeholders(cp)).toEqual(["CRAWLPROOF_API_KEY"]);
    const ok = expandServer(cp, (n) => (n === "CRAWLPROOF_API_KEY" ? "k123" : undefined));
    expect(ok.server.headers!.Authorization).toBe("Bearer k123");
    expect(ok.missing).toEqual([]);
    expect(cp.headers!.Authorization).toBe("Bearer ${CRAWLPROOF_API_KEY}"); // original untouched
    expect(expandServer(cp, () => undefined).missing).toEqual(["CRAWLPROOF_API_KEY"]);
  });
});

describe("import from Claude Code / Claude Desktop", () => {
  test("reads top-level and per-project servers, skips existing names", () => {
    const d = scratch();
    const claude = join(d, ".claude.json");
    writeFileSync(
      claude,
      JSON.stringify({
        mcpServers: { readm3: { type: "http", url: "https://readm3.com/mcp" }, gh: { command: "npx", args: ["-y", "gh-mcp"], env: { GITHUB_TOKEN: "fake-token-value" } } },
        projects: { "/p": { mcpServers: { local: { command: "node", args: ["s.js"] } } } },
      }),
    );
    const desktop = join(d, "claude_desktop_config.json");
    writeFileSync(desktop, JSON.stringify({ mcpServers: { fs: { command: "npx", args: ["fs-mcp", "/"] } } }));
    const out = join(d, "mcps");
    const found = readClaudeServers([claude, desktop, join(d, "missing.json")], out);
    expect(found.map((s) => s.name).sort()).toEqual(["fs", "gh", "local", "readm3"]);
    const existing = loadMcpDir(FIXTURES).servers;
    const r = importServers(out, found, existing);
    expect(r.skipped).toEqual(["readm3"]);
    expect(r.added.sort()).toEqual(["fs", "gh", "local"]);
    expect(loadMcpDir(out).servers.find((s) => s.name === "gh")!.env).toEqual({ GITHUB_TOKEN: "fake-token-value" });
  });
});
