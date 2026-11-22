// Prompt commands: slash commands defined as markdown files in the repo's
// commands/ directory (frontmatter: name, description, argument-hint, pack).
// `/analyze <question>` becomes the command's prompt (added to the system
// turn) plus the question, then runs as a normal chat turn with the person's
// chat plugins as tools. Expansion happens on the server (parseChatRequest),
// so the web chat, the TUI, `c0mpute chat`, the /v1 API and the MCP `chat`
// tool all behave the same; clients only need the list (for /help and the
// web chat's "/" autocomplete): GET /api/v1/chat/commands.
//
// Adding a command = one markdown file in commands/, then
// `bun scripts/gen-chat-commands.ts` (a test fails while the generated file
// is stale). This file is copied verbatim into apps/tui/src/web/.

import { COMMAND_DEFS } from "./command-defs.generated";

export type CommandDef = {
  name: string;
  description: string;
  /** "<question>" = required argument, "[period]" = optional, "" = none. */
  argumentHint: string;
  /** Pack id ("finance") and its display title, when the command belongs to one. */
  pack?: string;
  packTitle?: string;
  /** The instruction text added to the system turn. */
  prompt: string;
};

type Msg = { role: "system" | "user" | "assistant"; content: string };

/** Names the chat surfaces already use for their own commands: a file may not take one. */
export const RESERVED_NAMES = new Set([
  "image", "img", "draw", "video", "clip", "vid", "music", "song", "album", "lp", "promo", "promo-video",
  "demo-video", "video-promo", "push", "connect", "plugin", "plugins", "help", "h", "?", "commands", "model",
  "models", "new", "reset", "clear", "history", "chats", "open", "resume", "jobs", "exit", "quit",
]);

const NAME_RE = /^[a-z][a-z0-9-]{0,31}$/;

/**
 * Parse one command file: `---` frontmatter (`key: value` lines, `#` comment
 * lines ignored) then the prompt body. Throws on anything malformed so a bad
 * file fails the generator and the tests, never production.
 */
export function parseCommandFile(src: string, file = "command"): CommandDef {
  const m = /^---\r?\n([\s\S]*?)\r?\n---\r?\n([\s\S]*)$/.exec(src.replace(/^﻿/, ""));
  if (!m) throw new Error(`${file}: missing --- frontmatter`);
  const meta: Record<string, string> = {};
  for (const line of m[1].split(/\r?\n/)) {
    if (!line.trim() || line.trimStart().startsWith("#")) continue;
    const kv = /^([a-z][a-z-]*):\s*(.*)$/.exec(line.trim());
    if (!kv) throw new Error(`${file}: bad frontmatter line: ${line}`);
    let v = kv[2].trim();
    if ((v.startsWith('"') && v.endsWith('"')) || (v.startsWith("'") && v.endsWith("'"))) v = v.slice(1, -1);
    meta[kv[1]] = v;
  }
  const name = meta.name ?? "";
  if (!NAME_RE.test(name)) throw new Error(`${file}: name must match ${NAME_RE}`);
  if (RESERVED_NAMES.has(name)) throw new Error(`${file}: /${name} is a built-in chat command`);
  if (!meta.description) throw new Error(`${file}: description is required`);
  const prompt = m[2].trim();
  if (!prompt) throw new Error(`${file}: empty prompt`);
  const def: CommandDef = { name, description: meta.description, argumentHint: meta["argument-hint"] ?? "", prompt };
  if (meta.pack) def.pack = meta.pack;
  if (meta["pack-title"]) def.packTitle = meta["pack-title"];
  return def;
}

export const COMMANDS: readonly CommandDef[] = COMMAND_DEFS;

export function findCommand(name: string): CommandDef | undefined {
  const n = name.toLowerCase();
  return COMMANDS.find((c) => c.name === n);
}

/** "/analyze <question>" */
export const commandUsage = (c: CommandDef) => `/${c.name}${c.argumentHint ? ` ${c.argumentHint}` : ""}`;

/** True when the command needs an argument ("<...>" hint). */
export const needsArgument = (c: CommandDef) => c.argumentHint.trim().startsWith("<");

/** `/name rest` → the command and its argument, or null when the line is not one of ours. */
export function splitCommand(text: string): { command: CommandDef; arg: string } | null {
  const m = /^\s*\/([A-Za-z][\w-]*)(?:[ \t\r\n]+([\s\S]*))?$/.exec(String(text ?? ""));
  if (!m) return null;
  const command = findCommand(m[1]);
  return command ? { command, arg: (m[2] ?? "").trim() } : null;
}

/** The [usage, description] rows /help shows for prompt commands. */
export function commandRows(): [string, string][] {
  return COMMANDS.map((c) => [commandUsage(c), c.description]);
}

/**
 * The web chat's "/" autocomplete: every command whose name starts with what
 * was typed after the slash (only while the line is a bare "/word").
 */
export function completeCommands<T extends { name: string }>(input: string, all: readonly T[]): T[] {
  const m = /^\/([\w-]*)$/.exec(input);
  if (!m) return [];
  const q = m[1].toLowerCase();
  return all.filter((c) => c.name.toLowerCase().startsWith(q));
}

/** The public list (GET /api/v1/chat/commands, the MCP chat tool's description). */
export function publicCommands() {
  return COMMANDS.map((c) => ({
    name: c.name,
    usage: commandUsage(c),
    description: c.description,
    argument_hint: c.argumentHint,
    ...(c.pack ? { pack: c.pack, pack_title: c.packTitle ?? c.pack } : {}),
  }));
}

/**
 * Expand prompt commands in a conversation. Every user turn that starts with
 * a registered `/name` keeps only its argument; the prompt of the LATEST
 * command used goes into the system turn (appended to the caller's system
 * prompt, or after `housePrompt` when there is none), so follow-up questions
 * keep the command's instructions. Conversations without a command come back
 * unchanged (same array).
 */
export function expandCommands<M extends Msg>(messages: M[], housePrompt: string): M[] {
  let last: CommandDef | null = null;
  const out = messages.map((msg) => {
    if (msg.role !== "user") return msg;
    const hit = splitCommand(msg.content);
    if (!hit) return msg;
    last = hit.command;
    const content = hit.arg || `(/${hit.command.name} with no details: use the defaults, or ask me what to look at)`;
    return { ...msg, content };
  });
  if (!last) return messages;
  const cmd: CommandDef = last;
  const block = `/${cmd.name} instructions:\n${cmd.prompt}`;
  const i = out.findIndex((m) => m.role === "system");
  if (i >= 0) out[i] = { ...out[i], content: `${out[i].content}\n\n${block}` };
  else out.unshift({ role: "system", content: `${housePrompt}\n\n${block}` } as M);
  return out;
}

/** The pack disclaimer every finance answer must end with. */
export const FINANCE_DISCLAIMER = "Analysis, not financial, tax or legal advice.";
