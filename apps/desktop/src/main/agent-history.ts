// The Agent chat's conversation on disk, so it survives quitting the app:
// ~/.c0mpute/desktop-agent-chat.json (0600). Local only on purpose: agent
// turns can hold local file contents and shell output, so they never go to
// the c0mpute account. Electron-free for tests.

import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import type { AgentEvent, ChatMessage } from "./agent";
import { extractToolCalls } from "./tool-protocol";

/** Older turns beyond this are dropped from the file (the model's context is far smaller anyway). */
export const MAX_SAVED = 400;

export function agentHistoryPath(): string {
  return join(process.env.HOME || homedir(), ".c0mpute", "desktop-agent-chat.json");
}

export function loadAgentHistory(path = agentHistoryPath()): ChatMessage[] {
  if (!existsSync(path)) return [];
  try {
    const raw = JSON.parse(readFileSync(path, "utf8")) as { messages?: unknown };
    const list = Array.isArray(raw?.messages) ? raw.messages : [];
    return list
      .filter((m): m is ChatMessage => !!m && typeof m === "object" && ["user", "assistant", "system"].includes((m as ChatMessage).role) && typeof (m as ChatMessage).content === "string")
      .map(({ role, content }) => ({ role, content }))
      .slice(-MAX_SAVED);
  } catch {
    return [];
  }
}

export function saveAgentHistory(messages: ChatMessage[], path = agentHistoryPath()): void {
  mkdirSync(dirname(path), { recursive: true });
  const tmp = `${path}.tmp-${process.pid}`;
  writeFileSync(tmp, `${JSON.stringify({ v: 1, saved: Date.now(), messages: messages.slice(-MAX_SAVED) })}\n`, { mode: 0o600 });
  renameSync(tmp, path);
}

const RESULT = /^<tool_result name=("(?:[^"\\]|\\.)*") id=("(?:[^"\\]|\\.)*")( error="true")?>\n([\s\S]*?)\n<\/tool_result>/gm;

/**
 * A saved conversation as transcript events for the renderer: user and
 * assistant turns, tool calls and results as compact rows (the raw
 * `<tool_call>` / `<tool_result>` text the model sees is not shown).
 */
export function historyEvents(messages: ChatMessage[]): AgentEvent[] {
  const out: AgentEvent[] = [];
  let n = 0;
  for (const m of messages) {
    if (m.role === "system") continue;
    if (m.role === "user") {
      const results = [...m.content.matchAll(RESULT)];
      if (!results.length) {
        out.push({ type: "user", text: m.content });
        continue;
      }
      for (const r of results) {
        let name = "";
        let id = "";
        try {
          name = JSON.parse(r[1]);
          id = JSON.parse(r[2]);
        } catch {}
        const text = r[4].length > 600 ? `${r[4].slice(0, 600)}…` : r[4];
        out.push({ type: "tool_result", id: id || `h${n++}`, name, output: text, error: !!r[3] });
      }
      continue;
    }
    const parsed = extractToolCalls(m.content);
    if (parsed.text) out.push({ type: "assistant", text: parsed.text });
    for (const c of parsed.calls) {
      out.push({ type: "tool_call", id: c.id || `h${n++}`, name: c.name || "(unparsed)", source: "history", arguments: c.arguments ?? {} });
    }
  }
  return out;
}
