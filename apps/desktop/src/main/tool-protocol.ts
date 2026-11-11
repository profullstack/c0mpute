// Tool calling over plain chat messages.
//
// c0mpute.com/v1/chat/completions forwards only {role, content} messages with
// role system|user|assistant (apps/web/src/lib/chat/completions.ts strips
// `tools`), so the agent describes its tools in the system prompt and the
// model calls one by writing
//
//   <tool_call>{"name": "...", "arguments": {...}}</tool_call>
//
// (the Hermes / Qwen convention most open models were tuned on). Results go
// back as a user message wrapped in <tool_result>. A response that does carry
// OpenAI `tool_calls` is honoured too, so nothing changes if the API starts
// passing tools through.

export interface ToolDef {
  name: string;
  description: string;
  /** JSON Schema for the arguments object. */
  parameters: Record<string, unknown>;
}

export interface ToolCall {
  id: string;
  name: string;
  arguments: Record<string, unknown>;
  /** Set when the call could not be parsed; the model is told why. */
  error?: string;
}

let seq = 0;
const nextId = () => `call_${Date.now().toString(36)}_${(seq++).toString(36)}`;

function parseArgs(raw: unknown): Record<string, unknown> {
  if (raw == null || raw === "") return {};
  if (typeof raw === "string") {
    const v = JSON.parse(raw);
    if (v && typeof v === "object" && !Array.isArray(v)) return v;
    throw new Error("arguments must be a JSON object");
  }
  if (typeof raw === "object" && !Array.isArray(raw)) return raw as Record<string, unknown>;
  throw new Error("arguments must be a JSON object");
}

function callFromJson(text: string): ToolCall {
  const body = text.trim().replace(/^```(?:json)?\s*/i, "").replace(/\s*```$/, "");
  try {
    const v = JSON.parse(body) as Record<string, unknown>;
    const name = typeof v.name === "string" ? v.name : typeof v.tool === "string" ? v.tool : "";
    if (!name) throw new Error('no "name"');
    return { id: nextId(), name, arguments: parseArgs(v.arguments ?? v.args ?? v.parameters ?? v.input) };
  } catch (e) {
    return { id: nextId(), name: "", arguments: {}, error: `could not parse tool call: ${e instanceof Error ? e.message : e}` };
  }
}

/** Tool calls in a model reply, and the reply text without them. */
export function extractToolCalls(text: string): { calls: ToolCall[]; text: string } {
  const calls: ToolCall[] = [];
  let rest = text.replace(/<tool_call>([\s\S]*?)<\/tool_call>/g, (_, inner: string) => {
    calls.push(callFromJson(inner));
    return "";
  });
  // An unclosed final tag (the model stopped at its stop sequence).
  const open = rest.lastIndexOf("<tool_call>");
  if (open !== -1) {
    calls.push(callFromJson(rest.slice(open + "<tool_call>".length)));
    rest = rest.slice(0, open);
  }
  // Thinking models: drop <think> blocks from what the user sees.
  rest = rest.replace(/<think>[\s\S]*?<\/think>/g, "");
  return { calls, text: rest.trim() };
}

/** OpenAI-style `message.tool_calls`. */
export function nativeToolCalls(message: unknown): ToolCall[] {
  const m = message as { tool_calls?: { id?: string; function?: { name?: string; arguments?: unknown } }[] } | undefined;
  if (!m?.tool_calls?.length) return [];
  return m.tool_calls.map((c) => {
    try {
      return { id: c.id ?? nextId(), name: c.function?.name ?? "", arguments: parseArgs(c.function?.arguments) };
    } catch (e) {
      return { id: c.id ?? nextId(), name: c.function?.name ?? "", arguments: {}, error: String(e) };
    }
  });
}

export function toolsPrompt(tools: ToolDef[]): string {
  if (!tools.length) return "";
  const list = tools.map((t) => JSON.stringify({ name: t.name, description: t.description, parameters: t.parameters })).join("\n");
  return [
    "## Tools",
    "You can call tools. To call one, reply with a block exactly like this and nothing after it:",
    '<tool_call>{"name": "<tool name>", "arguments": {<arguments as JSON>}}</tool_call>',
    "You may put several <tool_call> blocks in one reply. Each result comes back in a <tool_result> block; then continue. When you have the answer, reply normally without any <tool_call>.",
    "Available tools (JSON Schema parameters):",
    list,
  ].join("\n");
}

const MAX_RESULT_CHARS = 24_000;

export function clip(s: string, max = MAX_RESULT_CHARS): string {
  return s.length > max ? `${s.slice(0, max)}\n… (${s.length - max} more characters cut)` : s;
}

export function formatToolResult(call: ToolCall, output: string, isError: boolean): string {
  return `<tool_result name=${JSON.stringify(call.name)} id=${JSON.stringify(call.id)}${isError ? ' error="true"' : ""}>\n${clip(output)}\n</tool_result>`;
}
