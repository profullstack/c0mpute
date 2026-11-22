// OpenAI tool calling on /v1/chat/completions.
//
// Nodes run models under Ollama behind Infernet, and that path carries plain
// string messages only, so tools are translated here, at the API edge, into
// the format the models were trained on: the Hermes / Qwen `<tool_call>`
// convention, which is exactly what Ollama's own Qwen chat templates render
// when it is given `tools`. History goes the same way (assistant tool_calls
// become `<tool_call>` blocks, role "tool" results become `<tool_response>`
// blocks in a user turn, as the Qwen template does), and the reply is parsed
// back into `choices[0].message.tool_calls`. Because nothing changes on the
// node, every node version serves tool requests and the auction, billing and
// house fee see an ordinary chat job.
//
// Only models known to follow the format take tool requests (TOOL_MODELS);
// any other named model gets a 400, never a reply that silently ignored the
// tools. docs: apps/web/src/app/docs (API section) and public/llms.txt.

import type { ChatMessage } from "./completions";

export type ToolDef = { name: string; description?: string; parameters: Record<string, unknown> };
export type ToolChoice = "auto" | "none" | "required" | { name: string };
export type Tooling = { tools: ToolDef[]; choice: ToolChoice; parallel: boolean };
export type ToolCall = { id: string; type: "function"; function: { name: string; arguments: string } };

const NAME_RE = /^[a-zA-Z0-9_.-]{1,64}$/;
export const MAX_TOOLS = 128;

// ── which models take tools ─────────────────────────────────────────────

/**
 * Model families that follow the `<tool_call>` format reliably. Measured on
 * the live network 2026-11-22: qwen3:4b and qwen2.5-coder:7b answer a
 * get_weather call correctly; the rest share the Qwen / Hermes training
 * format or Ollama ships a tools template for them. Small (<2B) models,
 * vision models, gemma, deepseek-coder-v2 and the profullstack fine-tunes
 * are left out: they answer in prose or invent the result.
 */
const TOOL_FAMILIES: RegExp[] = [
  /^qwen3(\.\d+)?(-coder)?:/,
  /^qwen2\.5(-coder)?:(3b|7b|14b|32b|72b)/,
  /^huihui_ai\/qwen3(\.\d+)?-abliterated:(2b|4b|9b|27b)/,
  /^eriktonon\/qwen3\.\d+-uncensored:/,
  /^nemotron-3-nano:/,
  /^devstral:/,
  /^mistral-nemo/,
  /^mistral-small/,
  /^llama3\.(1|2|3)(:|$)/,
  /^hermes3/,
];

export function supportsTools(model: string): boolean {
  const m = model.toLowerCase();
  return TOOL_FAMILIES.some((re) => re.test(m)) && !/:(0\.5b|0\.8b|1\.5b|1b)$/.test(m);
}

/**
 * Order tried for `model: "auto"` with tools, best first, among the live
 * models. Small CPU models first for fast/standard (they answer in ~30 s and
 * are free-tier); bigger ones first for high/max. Uncensored builds are never
 * picked for auto. A model that fails ("Network is busy", a node error)
 * hands the request to the next one.
 */
const AUTO_ORDER: Record<"fast" | "high", string[]> = {
  fast: ["qwen3:4b", "nemotron-3-nano:4b", "qwen2.5-coder:7b", "qwen2.5:7b", "qwen3:8b", "qwen2.5-coder:14b", "qwen3-coder:30b", "devstral:24b"],
  high: ["qwen3-coder:30b", "qwen2.5-coder:14b", "devstral:24b", "qwen3:8b", "qwen2.5-coder:7b", "qwen3:4b", "nemotron-3-nano:4b"],
};

export function autoToolModels(live: string[], effort: string): string[] {
  const order = AUTO_ORDER[effort === "high" || effort === "max" ? "high" : "fast"];
  const set = new Set(live);
  const picked = order.filter((m) => set.has(m));
  // Any other live tool-capable model after the preferred ones.
  for (const m of live) {
    if (!picked.includes(m) && supportsTools(m) && !/abliterated|uncensored/i.test(m)) picked.push(m);
  }
  return picked;
}

// ── request side ─────────────────────────────────────────────────────────

/** Validate `tools`, `tool_choice` and `parallel_tool_calls`. null = no tools in play. */
export function parseTooling(b: Record<string, unknown>): Tooling | null | string {
  if (b.tools === undefined || b.tools === null) {
    if (b.tool_choice !== undefined && b.tool_choice !== null && b.tool_choice !== "none" && b.tool_choice !== "auto") {
      return "tool_choice needs tools[]";
    }
    return null;
  }
  if (!Array.isArray(b.tools)) return "tools must be an array";
  if (b.tools.length === 0) return null;
  if (b.tools.length > MAX_TOOLS) return `at most ${MAX_TOOLS} tools`;
  const tools: ToolDef[] = [];
  const names = new Set<string>();
  for (const t of b.tools as Record<string, unknown>[]) {
    if (!t || typeof t !== "object" || t.type !== "function" || !t.function || typeof t.function !== "object") {
      return 'each tool must be { type: "function", function: { name, description?, parameters? } }';
    }
    const f = t.function as Record<string, unknown>;
    if (typeof f.name !== "string" || !NAME_RE.test(f.name)) return "tool function.name must match ^[a-zA-Z0-9_.-]{1,64}$";
    if (names.has(f.name)) return `duplicate tool name "${f.name}"`;
    names.add(f.name);
    if (f.parameters !== undefined && (typeof f.parameters !== "object" || f.parameters === null || Array.isArray(f.parameters))) {
      return `tool "${f.name}": parameters must be a JSON Schema object`;
    }
    tools.push({
      name: f.name,
      ...(typeof f.description === "string" && f.description ? { description: f.description.slice(0, 4000) } : {}),
      parameters: (f.parameters as Record<string, unknown>) ?? { type: "object", properties: {} },
    });
  }
  let choice: ToolChoice = "auto";
  const tc = b.tool_choice;
  if (tc === undefined || tc === null || tc === "auto") choice = "auto";
  else if (tc === "none" || tc === "required") choice = tc;
  else if (typeof tc === "object" && (tc as Record<string, unknown>).type === "function") {
    const name = ((tc as Record<string, unknown>).function as Record<string, unknown> | undefined)?.name;
    if (typeof name !== "string" || !names.has(name)) return "tool_choice.function.name must name one of tools[]";
    choice = { name };
  } else return 'tool_choice must be "auto", "none", "required" or { type: "function", function: { name } }';
  return { tools, choice, parallel: b.parallel_tool_calls !== false };
}

/** Text of an OpenAI content value: a string, null, or an array of parts. Image/audio/file parts are refused. */
export function contentText(c: unknown, role: string): string | { error: string } {
  if (typeof c === "string") return c;
  if (c === null || c === undefined) return "";
  if (!Array.isArray(c)) return { error: `${role} message content must be a string or an array of content parts` };
  const out: string[] = [];
  for (const p of c as Record<string, unknown>[]) {
    if (!p || typeof p !== "object") return { error: "content parts must be objects" };
    if ((p.type === "text" || p.type === "input_text") && typeof p.text === "string") out.push(p.text);
    else if (p.type === "image_url" || p.type === "input_image" || p.type === "image") {
      return { error: "image input is not supported: no vision model serves the c0mpute API yet; send text parts only" };
    } else if (p.type === "input_audio" || p.type === "file" || p.type === "audio") {
      return { error: `content part type "${String(p.type)}" is not supported; send text parts only` };
    } else if (p.type === "refusal" && typeof p.refusal === "string") out.push(p.refusal);
    else return { error: `unknown content part type "${String(p.type)}"` };
  }
  return out.join("\n");
}

function argsObject(a: unknown): unknown {
  if (typeof a !== "string") return a ?? {};
  try {
    return JSON.parse(a);
  } catch {
    return a;
  }
}

/** One `<tool_call>` block, the Hermes / Qwen format. */
export function toolCallBlock(name: string, args: unknown): string {
  return `<tool_call>\n${JSON.stringify({ name, arguments: argsObject(args) })}\n</tool_call>`;
}

/**
 * OpenAI messages (any role, tool_calls, tool results, content parts) to the
 * plain system/user/assistant strings Infernet carries. Returns an error
 * string for anything malformed.
 */
export function renderMessages(raw: unknown[]): ChatMessage[] | string {
  const out: ChatMessage[] = [];
  const callNames = new Map<string, string>();
  for (const m of raw as Record<string, unknown>[]) {
    if (!m || typeof m !== "object" || typeof m.role !== "string") return "each message must be an object with a role";
    const role = m.role;
    if (role === "system" || role === "developer") {
      const t = contentText(m.content, role);
      if (typeof t !== "string") return t.error;
      out.push({ role: "system", content: t });
    } else if (role === "user") {
      const t = contentText(m.content, role);
      if (typeof t !== "string") return t.error;
      out.push({ role: "user", content: t });
    } else if (role === "assistant") {
      const t = contentText(m.content, role);
      if (typeof t !== "string") return t.error;
      const parts = t ? [t] : [];
      if (m.tool_calls !== undefined && m.tool_calls !== null) {
        if (!Array.isArray(m.tool_calls)) return "assistant tool_calls must be an array";
        for (const c of m.tool_calls as Record<string, unknown>[]) {
          const f = c?.function as Record<string, unknown> | undefined;
          if (!f || typeof f.name !== "string") return "each assistant tool_call needs function.name";
          if (typeof c.id === "string") callNames.set(c.id, f.name);
          parts.push(toolCallBlock(f.name, f.arguments));
        }
      } else if (m.function_call && typeof m.function_call === "object") {
        const f = m.function_call as Record<string, unknown>;
        if (typeof f.name === "string") parts.push(toolCallBlock(f.name, f.arguments));
      }
      out.push({ role: "assistant", content: parts.join("\n") });
    } else if (role === "tool" || role === "function") {
      const t = contentText(m.content, role);
      if (typeof t !== "string") return t.error;
      if (role === "tool" && m.tool_call_id !== undefined && typeof m.tool_call_id !== "string") return "tool message tool_call_id must be a string";
      const id = typeof m.tool_call_id === "string" ? m.tool_call_id : undefined;
      const name = (typeof m.name === "string" && m.name) || (id ? callNames.get(id) : undefined);
      const block = `<tool_response>\n${JSON.stringify({ ...(name ? { name } : {}), content: argsObject(t) })}\n</tool_response>`;
      // Consecutive results share one user turn, as the Qwen template renders them.
      const last = out[out.length - 1];
      if (last && last.role === "user" && last.content.startsWith("<tool_response>")) last.content += `\n${block}`;
      else out.push({ role: "user", content: block });
    } else {
      return "each message role must be system, developer, user, assistant or tool";
    }
  }
  return out;
}

/** The system-prompt block that offers the tools (Qwen 2.5 / Hermes wording). */
export function toolsPrompt(t: Tooling): string {
  const offered = typeof t.choice === "object" ? t.tools.filter((x) => x.name === (t.choice as { name: string }).name) : t.tools;
  const sigs = offered.map((x) => JSON.stringify({ type: "function", function: x })).join("\n");
  const lines = [
    "# Tools",
    "",
    "You may call one or more functions to assist with the user query.",
    "",
    "You are provided with function signatures within <tools></tools> XML tags:",
    "<tools>",
    sigs,
    "</tools>",
    "",
    "For each function call, return a json object with function name and arguments within <tool_call></tool_call> XML tags:",
    "<tool_call>",
    '{"name": <function-name>, "arguments": <args-json-object>}',
    "</tool_call>",
    "",
    "Results come back to you inside <tool_response></tool_response> tags. Never write a <tool_response> yourself and never guess a result: call the function and wait for it.",
  ];
  if (t.choice === "required") lines.push("You must call at least one function now: reply with <tool_call> blocks only.");
  else if (typeof t.choice === "object") lines.push(`You must call the function "${t.choice.name}" now: reply with exactly one <tool_call> block for it and nothing else.`);
  else lines.push("If no function is needed, or the results you already have answer the question, reply to the user directly with no <tool_call>.");
  if (!t.parallel) lines.push("Call at most one function per reply.");
  return lines.join("\n");
}

/** Put the tools block into the conversation's system prompt (after the caller's own, or after `base`). */
export function withToolsPrompt(messages: ChatMessage[], t: Tooling, base: string): ChatMessage[] {
  const block = toolsPrompt(t);
  const i = messages.findIndex((m) => m.role === "system");
  if (i < 0) return [{ role: "system", content: `${base}\n\n${block}` }, ...messages];
  const copy = messages.slice();
  copy[i] = { role: "system", content: `${copy[i].content}\n\n${block}` };
  return copy;
}

// ── response side ────────────────────────────────────────────────────────

export function callId(): string {
  const abc = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
  let s = "call_";
  const r = crypto.getRandomValues(new Uint8Array(24));
  for (const b of r) s += abc[b % abc.length];
  return s;
}

/** Balanced `{...}` spans in text, string-aware. */
export function jsonSpans(text: string): { start: number; end: number }[] {
  const spans: { start: number; end: number }[] = [];
  let depth = 0;
  let start = -1;
  let inStr = false;
  let esc = false;
  for (let i = 0; i < text.length; i++) {
    const ch = text[i];
    if (inStr) {
      if (esc) esc = false;
      else if (ch === "\\") esc = true;
      else if (ch === '"') inStr = false;
      continue;
    }
    if (ch === '"' && depth > 0) inStr = true;
    else if (ch === "{") {
      if (depth === 0) start = i;
      depth++;
    } else if (ch === "}" && depth > 0) {
      depth--;
      if (depth === 0) spans.push({ start, end: i + 1 });
    }
  }
  return spans;
}

/** JSON.parse, then one pass of common small-model slips (trailing commas, single quotes, Python literals). */
export function looseJson(s: string): unknown {
  try {
    return JSON.parse(s);
  } catch {
    /* repair below */
  }
  let r = s.replace(/,\s*([}\]])/g, "$1");
  if (!r.includes('"') && r.includes("'")) r = r.replace(/'/g, '"');
  r = r.replace(/:\s*True\b/g, ": true").replace(/:\s*False\b/g, ": false").replace(/:\s*None\b/g, ": null");
  try {
    return JSON.parse(r);
  } catch {
    return undefined;
  }
}

/** `{name, arguments}` in any of the shapes models emit, or null. */
function asCall(v: unknown, names: Set<string>): { name: string; args: unknown } | null {
  if (!v || typeof v !== "object" || Array.isArray(v)) return null;
  let o = v as Record<string, unknown>;
  if (o.function && typeof o.function === "object") o = { ...(o.function as Record<string, unknown>) };
  const rawName = typeof o.name === "string" ? o.name : typeof o.tool === "string" ? o.tool : typeof o.tool_name === "string" ? o.tool_name : null;
  if (!rawName) return null;
  let name = rawName.trim();
  if (!names.has(name)) {
    const ci = [...names].find((n) => n.toLowerCase() === name.toLowerCase());
    if (!ci) return null;
    name = ci;
  }
  const args = o.arguments ?? o.parameters ?? o.args ?? o.input ?? {};
  return { name, args: typeof args === "string" ? (looseJson(args) ?? args) : args };
}

export type ParsedReply = { content: string | null; calls: ToolCall[]; attempted: boolean; maybe?: boolean };

/**
 * A model's raw text to OpenAI `content` + `tool_calls`. Reads `<tool_call>`
 * blocks (JSON, or the `<function=name><parameter=k>` form some models use),
 * and failing those a bare / fenced JSON object naming a declared tool.
 * `attempted` = the text looked like a tool call that could not be read.
 */
export function parseToolReply(raw: string, tools: ToolDef[]): ParsedReply {
  const names = new Set(tools.map((t) => t.name));
  let text = raw.replace(/<think>[\s\S]*?<\/think>/g, "").replace(/^[\s\S]*?<\/think>/, "");
  const calls: { name: string; args: unknown }[] = [];
  const cut: { start: number; end: number }[] = [];
  // attempted: tags only a tool call writes. maybe: JSON naming a tool (worth a repair, not an error).
  const attempted = /<tool_call>|<function=|<\|tool_call/.test(text);
  let maybe = false;

  // 1. <tool_call> ... </tool_call> (or to the end, when the model stopped early).
  const blockRe = /<tool_call>([\s\S]*?)(?:<\/tool_call>|(?=<tool_call>)|$)/g;
  for (let m; (m = blockRe.exec(text)); ) {
    const inner = m[1];
    let found = false;
    for (const sp of jsonSpans(inner)) {
      const c = asCall(looseJson(inner.slice(sp.start, sp.end)), names);
      if (c) {
        calls.push(c);
        found = true;
      }
    }
    if (!found) {
      const x = xmlCall(inner, names);
      if (x) calls.push(x);
    }
    cut.push({ start: m.index, end: m.index + m[0].length });
    if (m[0].length === 0) blockRe.lastIndex++;
  }

  // 2. <function=name><parameter=k>v</parameter></function> outside a block.
  if (!calls.length) {
    const fnRe = /<function=([^>\s]+)>([\s\S]*?)(?:<\/function>|$)/g;
    for (let m; (m = fnRe.exec(text)); ) {
      const x = xmlCall(m[0], names);
      if (x) {
        calls.push(x);
        cut.push({ start: m.index, end: m.index + m[0].length });
      }
    }
  }

  // 3. A bare or fenced JSON object that names a declared tool and has arguments.
  if (!calls.length) {
    for (const sp of jsonSpans(text)) {
      const v = looseJson(text.slice(sp.start, sp.end));
      const list = v && typeof v === "object" && Array.isArray((v as Record<string, unknown>).tool_calls) ? ((v as Record<string, unknown>).tool_calls as unknown[]) : [v];
      let hit = false;
      for (const item of list) {
        const o = item as Record<string, unknown> | undefined;
        const hasArgs = !!o && typeof o === "object" && ("arguments" in o || "parameters" in o || (typeof o.function === "object" && o.function !== null));
        const c = hasArgs ? asCall(item, names) : null;
        if (c) {
          calls.push(c);
          hit = true;
        }
      }
      if (hit) cut.push(sp);
    }
    if (!calls.length && [...names].some((n) => new RegExp(`"name"\\s*:\\s*"${n.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}"`).test(text))) maybe = true;
  }

  // What is left is the visible content.
  cut.sort((a, b) => b.start - a.start);
  for (const c of cut) text = text.slice(0, c.start) + text.slice(c.end);
  text = text
    .replace(/<\/?tool_call>|<\/?json>|<\/?tools?>|<\|[a-z_]+\|>/g, "")
    .replace(/```(?:json)?\s*```/g, "")
    .trim();

  return {
    content: text ? text : null,
    calls: calls.map((c) => ({ id: callId(), type: "function", function: { name: c.name, arguments: typeof c.args === "string" ? c.args : JSON.stringify(c.args ?? {}) } })),
    attempted: attempted && calls.length === 0,
    maybe: maybe && calls.length === 0,
  };
}

function xmlCall(s: string, names: Set<string>): { name: string; args: unknown } | null {
  const fm = /<function=([^>\s]+)>/.exec(s);
  if (!fm) return null;
  const c = asCall({ name: fm[1], arguments: {} }, names);
  if (!c) return null;
  const args: Record<string, unknown> = {};
  const pRe = /<parameter=([^>\s]+)>([\s\S]*?)(?:<\/parameter>|(?=<parameter=)|(?=<\/function>)|$)/g;
  for (let m; (m = pRe.exec(s)); ) {
    const v = m[2].trim();
    const j = looseJson(v);
    args[m[1]] = j === undefined || typeof j === "string" ? v : j;
  }
  return { name: c.name, args };
}

/** Apply tool_choice / parallel_tool_calls to what the model returned. */
export function applyChoice(p: ParsedReply, t: Tooling): ParsedReply {
  let calls = p.calls;
  if (typeof t.choice === "object") calls = calls.filter((c) => c.function.name === (t.choice as { name: string }).name);
  if (!t.parallel) calls = calls.slice(0, 1);
  return { ...p, calls };
}

/** Whether the reply needs the one repair retry. */
export function needsRepair(p: ParsedReply, t: Tooling): boolean {
  if (p.calls.length) return false;
  return p.attempted || !!p.maybe || t.choice === "required" || typeof t.choice === "object";
}

export function repairMessage(t: Tooling): string {
  const target = typeof t.choice === "object" ? `the function "${t.choice.name}"` : "a function";
  return (
    `Your last reply did not contain a valid tool call. Call ${target} now. Reply with only:\n` +
    '<tool_call>\n{"name": "<function-name>", "arguments": {<arguments as a JSON object>}}\n</tool_call>'
  );
}

export type Usage = { prompt_tokens: number; completion_tokens: number; total_tokens: number };

/** Token counts are not reported by the nodes; ~4 characters a token. */
export function estimateUsage(prompt: ChatMessage[], completion: string): Usage {
  const p = Math.ceil(prompt.reduce((n, m) => n + m.content.length + 4, 0) / 4);
  const c = Math.ceil(completion.length / 4);
  return { prompt_tokens: p, completion_tokens: c, total_tokens: p + c };
}

export function toolCompletionJson(p: ParsedReply, model: string, usage: Usage, id = `chatcmpl-c0t-${Date.now().toString(36)}`) {
  return {
    id,
    object: "chat.completion",
    created: Math.floor(Date.now() / 1000),
    model,
    choices: [
      {
        index: 0,
        message: { role: "assistant", content: p.content, ...(p.calls.length ? { tool_calls: p.calls } : {}) },
        finish_reason: p.calls.length ? "tool_calls" : "stop",
      },
    ],
    usage,
  };
}

/** OpenAI streaming chunks for a finished reply: role, content, per-call tool_calls deltas, finish, optional usage. */
export function toolStreamChunks(p: ParsedReply, model: string, usage: Usage | null, id = `chatcmpl-c0t-${Date.now().toString(36)}`): unknown[] {
  const created = Math.floor(Date.now() / 1000);
  const chunk = (delta: Record<string, unknown>, finish: string | null = null) => ({
    id,
    object: "chat.completion.chunk",
    created,
    model,
    choices: [{ index: 0, delta, finish_reason: finish }],
  });
  const out: unknown[] = [chunk({ role: "assistant", content: p.content === null ? null : "" })];
  if (p.content) for (let i = 0; i < p.content.length; i += 200) out.push(chunk({ content: p.content.slice(i, i + 200) }));
  p.calls.forEach((c, index) => {
    out.push(chunk({ tool_calls: [{ index, id: c.id, type: "function", function: { name: c.function.name, arguments: "" } }] }));
    const a = c.function.arguments;
    for (let i = 0; i < a.length; i += 64) out.push(chunk({ tool_calls: [{ index, function: { arguments: a.slice(i, i + 64) } }] }));
  });
  out.push(chunk({}, p.calls.length ? "tool_calls" : "stop"));
  if (usage) out.push({ id, object: "chat.completion.chunk", created, model, choices: [], usage });
  return out;
}
