import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import {
  COMMANDS,
  completeCommands,
  expandCommands,
  FINANCE_DISCLAIMER,
  findCommand,
  needsArgument,
  parseCommandFile,
  publicCommands,
  RESERVED_NAMES,
  splitCommand,
  usedCommand,
} from "./command-registry";
import { CHAT_COMMANDS, slashCommand, SUGGESTIONS } from "./commands";
import { parseChatRequest, C0_SYSTEM_PROMPT } from "./completions";
import { GENERATED_PATHS, loadCommandDir, renderGenerated } from "../../../../../scripts/gen-chat-commands";

const repo = join(import.meta.dir, "../../../../..");

describe("command files", () => {
  test("the generated registry matches commands/*.md (run `bun scripts/gen-chat-commands.ts`)", () => {
    const want = renderGenerated(loadCommandDir());
    for (const p of GENERATED_PATHS) expect(readFileSync(p, "utf8")).toBe(want);
    // The TUI's copy of this module is verbatim too.
    expect(readFileSync(join(repo, "apps/tui/src/web/command-registry.ts"), "utf8")).toBe(
      readFileSync(join(import.meta.dir, "command-registry.ts"), "utf8"),
    );
  });

  test("ships /analyze and the finance pack", () => {
    for (const n of ["analyze", "cashflow", "runway", "pnl", "spend", "forecast", "board"]) {
      const c = findCommand(n);
      expect(c, n).toBeDefined();
      expect(c!.pack).toBe("finance");
      expect(c!.packTitle).toBe("Agentic Advisor: Finance");
      expect(c!.prompt).toContain(FINANCE_DISCLAIMER);
      // Small models: keep every prompt well under 1k tokens.
      expect(c!.prompt.length, n).toBeLessThan(3000);
    }
    expect(findCommand("ANALYZE")?.argumentHint).toBe("<question>");
    expect(needsArgument(findCommand("analyze")!)).toBe(true);
    expect(needsArgument(findCommand("runway")!)).toBe(false);
  });

  test("/analyze keeps the Apache-2.0 attribution and no Claude-only references", () => {
    const src = readFileSync(join(repo, "commands/analyze.md"), "utf8");
    expect(src).toContain("Apache License 2.0");
    expect(src).toContain("knowledge-work-plugins");
    expect(readFileSync(join(repo, "commands/LICENSE-APACHE-2.0.txt"), "utf8")).toContain("Apache License");
    for (const c of COMMANDS) expect(c.prompt).not.toMatch(/Claude|CONNECTORS\.md|sql-queries|data-visualization|vu1nz/);
  });

  test("parser: frontmatter, comments, quotes, and refusals", () => {
    const d = parseCommandFile('---\nname: demo\n# a comment\ndescription: "Do a thing"\nargument-hint: "<x>"\n---\nPrompt body.\n');
    expect(d).toEqual({ name: "demo", description: "Do a thing", argumentHint: "<x>", prompt: "Prompt body." });
    expect(() => parseCommandFile("no frontmatter")).toThrow(/frontmatter/);
    expect(() => parseCommandFile("---\nname: image\ndescription: x\n---\nbody")).toThrow(/built-in/);
    expect(() => parseCommandFile("---\nname: Bad Name\ndescription: x\n---\nbody")).toThrow(/name/);
    expect(() => parseCommandFile("---\nname: ok\n---\nbody")).toThrow(/description/);
    expect(() => parseCommandFile("---\nname: ok\ndescription: x\n---\n  ")).toThrow(/empty/);
    for (const c of COMMANDS) expect(RESERVED_NAMES.has(c.name)).toBe(false);
  });
});

describe("autocomplete and /help", () => {
  test("typing / lists every command; a prefix narrows it", () => {
    const all = completeCommands("/", SUGGESTIONS).map((s) => s.name);
    expect(all).toContain("analyze");
    expect(all).toContain("image");
    expect(completeCommands("/an", SUGGESTIONS).map((s) => s.name)).toEqual(["analyze"]);
    expect(completeCommands("/r", SUGGESTIONS).map((s) => s.name)).toContain("runway");
    // Only while the line is a bare "/word".
    expect(completeCommands("/analyze why", SUGGESTIONS)).toEqual([]);
    expect(completeCommands("hello", SUGGESTIONS)).toEqual([]);
    expect(SUGGESTIONS.find((s) => s.name === "analyze")?.description).toMatch(/data question/);
  });

  test("/help lists the prompt commands", () => {
    expect(CHAT_COMMANDS.some(([u]) => u === "/analyze <question>")).toBe(true);
    expect(CHAT_COMMANDS[CHAT_COMMANDS.length - 1][0]).toBe("/help");
    expect(publicCommands().find((c) => c.name === "runway")).toMatchObject({ usage: "/runway [cash, burn or scenario]", pack: "finance" });
  });

  test("the web chat sends a prompt command to the model, or shows usage without its argument", () => {
    expect(slashCommand("/analyze how many signups in May?")).toBeNull();
    expect(slashCommand("/runway")).toBeNull();
    expect(slashCommand("/analyze")).toEqual({ kind: "usage", message: "usage: /analyze <question>" });
  });
});

describe("expansion", () => {
  test("splitCommand", () => {
    expect(splitCommand("/analyze  what changed?\nmonth,signups\n1,2")?.arg).toBe("what changed?\nmonth,signups\n1,2");
    expect(splitCommand("/Analyze\nmonth,signups")?.command.name).toBe("analyze");
    expect(splitCommand("/analyzer x")).toBeNull();
    expect(splitCommand("please /analyze x")).toBeNull();
  });

  test("the user turn keeps the question; the prompt joins the house system turn", () => {
    const out = expandCommands([{ role: "user" as const, content: "/analyze How many signups in total?" }], "HOUSE");
    expect(out).toHaveLength(2);
    expect(out[0].role).toBe("system");
    expect(out[0].content).toMatch(/^HOUSE\n\n\/analyze instructions \(today is \d{4}-\d{2}-\d{2}\):\n/);
    expect(out[0].content).toContain("**Checks**");
    expect(out[1]).toEqual({ role: "user", content: "How many signups in total?" });
  });

  test("a caller's system prompt is kept and extended; follow-ups keep the latest command", () => {
    const msgs = [
      { role: "system" as const, content: "MINE" },
      { role: "user" as const, content: "/cashflow last quarter" },
      { role: "assistant" as const, content: "table..." },
      { role: "user" as const, content: "/runway" },
      { role: "user" as const, content: "and if burn doubles?" },
    ];
    const out = expandCommands(msgs, "HOUSE");
    expect(out[0].content.startsWith("MINE\n\n/runway instructions (today is")).toBe(true);
    expect(out[0].content).not.toContain("/cashflow instructions");
    expect(out[1].content).toBe("last quarter");
    expect(out[3].content).toMatch(/^\(\/runway with no details/);
    expect(usedCommand(msgs)?.name).toBe("runway");
  });

  test("no command: same array back", () => {
    const msgs = [{ role: "user" as const, content: "/notacommand hi" }];
    expect(expandCommands(msgs, "HOUSE")).toBe(msgs);
  });

  test("parseChatRequest expands for every surface (API, CLI, MCP, TUI)", () => {
    const r = parseChatRequest({ messages: [{ role: "user", content: "/analyze total?\nmonth,n\nJan,3\nFeb,4" }] });
    if (typeof r === "string") throw new Error(r);
    expect(r.command).toBe("analyze");
    expect(r.messages[0].content.startsWith(C0_SYSTEM_PROMPT)).toBe(true);
    expect(r.messages[0].content).toContain("/analyze instructions (today is");
    expect(r.temperature).toBe(0.2);
    const warm = parseChatRequest({ temperature: 0.9, messages: [{ role: "user", content: "/runway" }] });
    if (typeof warm === "string") throw new Error(warm);
    expect(warm.temperature).toBe(0.9);
    expect(r.messages[1].content).toBe("total?\nmonth,n\nJan,3\nFeb,4");
    const plain = parseChatRequest({ messages: [{ role: "user", content: "hi" }] });
    if (typeof plain === "string") throw new Error(plain);
    expect(plain.command).toBeUndefined();
    expect(plain.messages).toEqual([{ role: "user", content: "hi" }]);
  });
});
