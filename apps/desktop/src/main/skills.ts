// ~/skills: Agent Skills (https://agentskills.io/specification). One folder per
// skill, `SKILL.md` = YAML front matter (`name`, `description`, optional
// `license`, `allowed-tools`, `metadata`) + the procedure, plus optional
// `scripts/`, `references/` and `assets/`.
//
// Like Claude Code, the agent sees only each enabled skill's name and
// description up front, and loads the full SKILL.md with `load_skill` when a
// task matches.

import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, realpathSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { isAbsolute, join, relative, resolve, sep } from "node:path";
import { unzipSync } from "fflate";
import { parse as parseYaml } from "yaml";

export interface Skill {
  name: string;
  description: string;
  dir: string;
  file: string;
  enabled: boolean;
  /** Files under scripts/, relative to the skill folder. */
  scripts: string[];
  /** Everything else besides SKILL.md, relative (capped). */
  resources: string[];
  error?: string;
}

export function defaultSkillsDir(): string {
  return process.env.C0MPUTE_SKILLS_DIR || join(homedir(), "skills");
}

/** Splits `---\nyaml\n---\nbody`. No front matter: empty data, whole text as body. */
export function parseFrontMatter(text: string): { data: Record<string, unknown>; body: string } {
  const m = /^﻿?---\r?\n([\s\S]*?)\r?\n---[ \t]*(?:\r?\n|$)/.exec(text);
  if (!m) return { data: {}, body: text };
  let data: unknown;
  try {
    data = parseYaml(m[1]);
  } catch (e) {
    throw new Error(`front matter is not valid YAML: ${e instanceof Error ? e.message : e}`);
  }
  return { data: data && typeof data === "object" && !Array.isArray(data) ? (data as Record<string, unknown>) : {}, body: text.slice(m[0].length) };
}

/** Spec: 1-64 chars, lowercase letters, digits and hyphens, no leading/trailing/double hyphen. */
export function validSkillName(n: string): boolean {
  return n.length >= 1 && n.length <= 64 && /^[a-z0-9]+(-[a-z0-9]+)*$/.test(n);
}

function listFiles(root: string, rel = "", out: string[] = [], cap = 200): string[] {
  const here = join(root, rel);
  let entries: string[];
  try {
    entries = readdirSync(here);
  } catch {
    return out;
  }
  for (const e of entries.sort()) {
    if (out.length >= cap) break;
    if (e.startsWith(".") || e === "node_modules") continue;
    const r = rel ? `${rel}/${e}` : e;
    let st;
    try {
      st = statSync(join(root, r));
    } catch {
      continue;
    }
    if (st.isDirectory()) listFiles(root, r, out, cap);
    else out.push(r);
  }
  return out;
}

export function readSkill(dir: string, enabled: boolean): Skill {
  const folder = dir.split(/[\\/]/).pop() ?? dir;
  const file = join(dir, "SKILL.md");
  const base: Skill = { name: folder, description: "", dir, file, enabled, scripts: [], resources: [] };
  if (!existsSync(file)) return { ...base, error: "no SKILL.md" };
  try {
    const { data } = parseFrontMatter(readFileSync(file, "utf8"));
    const name = typeof data.name === "string" ? data.name.trim() : folder;
    const description = typeof data.description === "string" ? data.description.trim() : "";
    const files = listFiles(dir).filter((f) => f !== "SKILL.md");
    const s: Skill = {
      ...base,
      name,
      description,
      scripts: files.filter((f) => f.startsWith("scripts/")),
      resources: files.filter((f) => !f.startsWith("scripts/")),
    };
    if (!description) s.error = "front matter has no description";
    else if (name !== folder) s.error = `name "${name}" does not match its folder "${folder}"`;
    return s;
  } catch (e) {
    return { ...base, error: e instanceof Error ? e.message : String(e) };
  }
}

export function loadSkills(dir = defaultSkillsDir(), disabled: Iterable<string> = []): Skill[] {
  if (!existsSync(dir)) return [];
  const off = new Set(disabled);
  return readdirSync(dir)
    .filter((n) => !n.startsWith(".") && statSync(join(dir, n)).isDirectory())
    .sort()
    .map((n) => readSkill(join(dir, n), !off.has(n)))
    .filter((s) => existsSync(s.file));
}

/** The system-prompt block listing the skills the agent may load. */
export function skillsPrompt(skills: Skill[]): string {
  const on = skills.filter((s) => s.enabled && !s.error);
  if (!on.length) return "";
  const lines = on.map((s) => `- ${s.name}: ${s.description.replace(/\s+/g, " ")}`);
  return [
    "## Skills",
    "These skills are available. When a task matches one, call load_skill with its name first and follow the procedure it returns; use read_skill_file for files it references and run_skill_script for its scripts.",
    ...lines,
  ].join("\n");
}

/** `rel` resolved under `root`, or null when it would escape it (also via symlinks). */
export function resolveInside(root: string, rel: string): string | null {
  if (!rel || isAbsolute(rel) || rel.includes("\0")) return null;
  const target = resolve(root, rel);
  const r = relative(resolve(root), target);
  if (r === "" || r.startsWith("..") || isAbsolute(r)) return null;
  if (existsSync(target)) {
    try {
      const real = realpathSync(target);
      const realRoot = realpathSync(root);
      if (real !== realRoot && !real.startsWith(realRoot + sep)) return null;
    } catch {
      return null;
    }
  }
  return target;
}

export function skillTemplate(name: string, description: string): string {
  return `---
name: ${name}
description: ${JSON.stringify(description || `What ${name} does and when to use it.`)}
---

# ${name}

## When to use

Describe the tasks this skill is for.

## Procedure

1. First step.
2. Second step.

Helper scripts go in \`scripts/\`; reference material in \`references/\`.
`;
}

export function createSkill(root: string, name: string, description: string): string {
  if (!validSkillName(name)) throw new Error("skill name: lowercase letters, digits and single hyphens, up to 64");
  const dir = join(root, name);
  if (existsSync(dir)) throw new Error(`${name} already exists`);
  mkdirSync(join(dir, "scripts"), { recursive: true });
  writeFileSync(join(dir, "SKILL.md"), skillTemplate(name, description));
  return dir;
}

/** Copy a skill folder in; its SKILL.md name decides the folder name. */
export function importSkillFolder(root: string, from: string): string {
  const s = readSkill(from, true);
  if (!existsSync(s.file)) throw new Error("that folder has no SKILL.md");
  const name = validSkillName(s.name) ? s.name : (from.split(/[\\/]/).pop() ?? "");
  if (!validSkillName(name)) throw new Error(`"${name}" is not a valid skill name`);
  const dest = join(root, name);
  if (existsSync(dest)) throw new Error(`${name} already exists`);
  cpSync(from, dest, { recursive: true, filter: (p) => !p.includes(`${sep}node_modules`) && !p.includes(`${sep}.git`) });
  return dest;
}

/**
 * Unpack a skill zip: either `<name>/SKILL.md ...` or `SKILL.md ...` at the
 * top. Entries that would land outside the new folder are refused.
 */
export function importSkillZip(root: string, zip: Uint8Array, fallbackName: string): string {
  const files = unzipSync(zip);
  const names = Object.keys(files).filter((n) => !n.endsWith("/") && !n.startsWith("__MACOSX/"));
  const skillMd = names.filter((n) => /(^|\/)SKILL\.md$/.test(n)).sort((a, b) => a.length - b.length)[0];
  if (!skillMd) throw new Error("the zip has no SKILL.md");
  const prefix = skillMd.slice(0, -"SKILL.md".length);
  const { data } = parseFrontMatter(new TextDecoder().decode(files[skillMd]));
  const name = typeof data.name === "string" && validSkillName(data.name) ? data.name : fallbackName;
  if (!validSkillName(name)) throw new Error(`"${name}" is not a valid skill name`);
  const dest = join(root, name);
  if (existsSync(dest)) throw new Error(`${name} already exists`);
  mkdirSync(dest, { recursive: true });
  for (const n of names) {
    if (!n.startsWith(prefix)) continue;
    const rel = n.slice(prefix.length);
    const target = resolveInside(dest, rel);
    if (!target) throw new Error(`refusing zip entry ${n}`);
    mkdirSync(join(target, ".."), { recursive: true });
    writeFileSync(target, files[n]);
  }
  return dest;
}
