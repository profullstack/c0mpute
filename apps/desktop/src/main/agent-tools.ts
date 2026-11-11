// The tools the Agent chat gets: every enabled MCP server's tools, the skill
// tools (load_skill, read_skill_file, run_skill_script) and the opt-in shell.

import { spawn } from "node:child_process";
import { closeSync, existsSync, fstatSync, openSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import type { AgentTool } from "./agent";
import type { McpManager } from "./mcp-client";
import { qualifiedName } from "./mcp-client";
import type { Settings } from "./settings";
import { resolveInside, type Skill } from "./skills";

export function runProcess(cmd: string, args: string[], opts: { cwd?: string; timeoutMs?: number; signal?: AbortSignal; shell?: boolean }): Promise<string> {
  return new Promise((resolve, reject) => {
    const child = spawn(cmd, args, { cwd: opts.cwd, env: process.env, shell: opts.shell, windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
    let out = "";
    const add = (d: Buffer) => {
      if (out.length < 200_000) out += d.toString();
    };
    child.stdout.on("data", add);
    child.stderr.on("data", add);
    const kill = () => child.kill("SIGTERM");
    const t = setTimeout(kill, opts.timeoutMs ?? 120_000);
    opts.signal?.addEventListener("abort", kill, { once: true });
    child.on("error", (e) => {
      clearTimeout(t);
      reject(e);
    });
    child.on("close", (code, sig) => {
      clearTimeout(t);
      const tail = sig ? `\n(killed by ${sig})` : code ? `\n(exit code ${code})` : "";
      resolve(`${out.trim()}${tail}` || "(no output)");
    });
  });
}

/** How to run a skill script by its extension. */
export function interpreterFor(file: string): { cmd: string; args: string[] } {
  const ext = file.split(".").pop()?.toLowerCase();
  switch (ext) {
    case "sh":
    case "bash":
      return { cmd: "bash", args: [file] };
    case "py":
      return { cmd: "python3", args: [file] };
    case "js":
    case "mjs":
    case "cjs":
      return { cmd: "node", args: [file] };
    case "ts":
      return { cmd: "bun", args: [file] };
    case "rb":
      return { cmd: "ruby", args: [file] };
    default:
      return { cmd: file, args: [] };
  }
}

export function buildTools(opts: { mcp: McpManager; skills: Skill[]; settings: Settings }): AgentTool[] {
  const { mcp, settings } = opts;
  const tools: AgentTool[] = [];
  for (const t of mcp.tools()) {
    tools.push({
      name: qualifiedName(t.server, t.name),
      description: `[MCP ${t.server}] ${t.description}`.slice(0, 1024),
      parameters: t.inputSchema,
      source: `mcp:${t.server}`,
      confirm: !settings.mcpAllowed.includes(t.server),
      run: async (args, signal) => {
        const r = await mcp.call(t.server, t.name, args, signal);
        if (r.isError) throw new Error(r.text);
        return r.text;
      },
    });
  }

  const skills = opts.skills.filter((s) => s.enabled && !s.error);
  if (skills.length) {
    const find = (name: unknown) => {
      const s = skills.find((x) => x.name === name);
      if (!s) throw new Error(`no enabled skill named ${JSON.stringify(name)}; enabled: ${skills.map((x) => x.name).join(", ")}`);
      return s;
    };
    tools.push({
      name: "load_skill",
      description: "Load a skill's full SKILL.md (its procedure) by name. Do this before following a skill.",
      parameters: { type: "object", properties: { name: { type: "string" } }, required: ["name"] },
      source: "skills",
      confirm: false,
      run: async (a) => {
        const s = find(a.name);
        const files = [...s.scripts, ...s.resources];
        return `${readFileSync(s.file, "utf8")}${files.length ? `\n\n(files in this skill: ${files.join(", ")})` : ""}`;
      },
    });
    tools.push({
      name: "read_skill_file",
      description: "Read a file inside a skill folder (a reference, template or script source), by skill name and relative path.",
      parameters: { type: "object", properties: { name: { type: "string" }, path: { type: "string" } }, required: ["name", "path"] },
      source: "skills",
      confirm: false,
      run: async (a) => {
        const s = find(a.name);
        const p = resolveInside(s.dir, String(a.path ?? ""));
        if (!p) throw new Error(`no file ${a.path} in ${s.name}`);
        // Open once and check the open file, so what is checked is what is read.
        let fd: number;
        try {
          fd = openSync(p, "r");
        } catch {
          throw new Error(`no file ${a.path} in ${s.name}`);
        }
        try {
          const st = fstatSync(fd);
          if (!st.isFile()) throw new Error(`no file ${a.path} in ${s.name}`);
          if (st.size > 512 * 1024) throw new Error("file is larger than 512 KiB");
          return readFileSync(fd, "utf8");
        } finally {
          closeSync(fd);
        }
      },
    });
    tools.push({
      name: "run_skill_script",
      description: "Run a script from a skill's scripts/ folder (the user confirms each run). Arguments are passed as an argv list.",
      parameters: {
        type: "object",
        properties: { name: { type: "string" }, script: { type: "string", description: "e.g. scripts/check.sh" }, args: { type: "array", items: { type: "string" } } },
        required: ["name", "script"],
      },
      source: "skills:scripts",
      confirm: !settings.skillScriptsAllowed,
      run: async (a, signal) => {
        const s = find(a.name);
        const rel = String(a.script ?? "");
        const p = resolveInside(s.dir, rel.startsWith("scripts/") ? rel : `scripts/${rel}`);
        if (!p || !existsSync(p)) throw new Error(`no script ${rel} in ${s.name}`);
        const argv = Array.isArray(a.args) ? a.args.map(String) : [];
        const { cmd, args } = interpreterFor(p);
        return runProcess(cmd, [...args, ...argv], { cwd: s.dir, signal });
      },
    });
  }

  if (settings.shellEnabled) {
    tools.push({
      name: "shell",
      description: "Run a shell command on the user's machine (the user confirms every command). Returns combined stdout and stderr.",
      parameters: { type: "object", properties: { command: { type: "string" }, cwd: { type: "string" } }, required: ["command"] },
      source: "shell",
      // Never allowed wholesale: every command is confirmed.
      confirm: true,
      run: async (a, signal) => {
        const command = String(a.command ?? "");
        if (!command.trim()) throw new Error("empty command");
        const cwd = typeof a.cwd === "string" && existsSync(a.cwd) ? a.cwd : homedir();
        return process.platform === "win32"
          ? runProcess("cmd.exe", ["/d", "/s", "/c", command], { cwd, signal })
          : runProcess("/bin/sh", ["-c", command], { cwd, signal });
      },
    });
  }
  return tools;
}
