// Context budgeting for chat on the c0mpute network.
//
// Clients (the PWA, the TUI, /v1 callers) resend the whole conversation every
// turn, and the plugin loop adds up to six tool results on top. Nodes load
// models with num_ctx 16384 or 32768 (node/crates/c0mpute-core/src/inference_gpu.rs
// ctx_cap_for_vram), well under the 200,000 characters parseChatRequest accepts,
// so a long chat used to reach the engine oversized and be cut there, blind to
// what mattered. This fits the prompt here first, in the "harness-driven"
// shape (arXiv 2611.34147, NVIDIA 2026): at 80% of the window, keep the
// instructions and the most recent turns verbatim and compact what is between.
//
// Order of work, cheapest information loss first:
//   1. nothing, while the prompt fits (the common case; the prompt is untouched);
//   2. clip old tool results (<tool_result> from chat plugins, <tool_response>
//      from client tools) to their head and tail: documents and API dumps are
//      what fill a window, and the model already acted on them;
//   3. drop the oldest middle turns, leaving a short extractive digest of the
//      dropped user turns in the system prompt so the model knows they existed.
// System turns, the first user turn (usually the task) and the newest turns are
// never touched. Deterministic and synchronous: no extra model call, no state.

import type { ChatMessage } from "./completions";

/** Context per answer on the nodes that serve the house models (>= 10 GiB cards). */
export const DEFAULT_CONTEXT_TOKENS = 32_768;
/** Compact once the prompt passes this share of what is left after the answer. */
export const TRIGGER_RATIO = 0.8;
/** Tokens kept free for the answer when the request sets no max_tokens. */
export const DEFAULT_ANSWER_RESERVE = 2_048;
/** Newest messages whose tool results are never clipped. */
const KEEP_TOOL_TAIL = 2;
/** What a clipped tool result keeps. */
const CLIP_HEAD = 1_200;
const CLIP_TAIL = 400;
/** Digest of dropped turns appended to the system prompt. */
const DIGEST_LINE = 160;
const DIGEST_MAX = 2_000;

export type ContextReport = {
  /** True when the prompt was changed to fit. */
  compacted: boolean;
  /** Estimated prompt tokens before and after. */
  before: number;
  after: number;
  /** The prompt budget it was fitted to. */
  budget: number;
  clippedResults: number;
  droppedMessages: number;
};

/** Rough tokens: 4 characters each plus a little per message, as tools.ts counts usage. */
export function estimateTokens(messages: ChatMessage[]): number {
  return Math.ceil(messages.reduce((n, m) => n + m.content.length + 16, 0) / 4);
}

/** Prompt tokens allowed for a window of `contextTokens` and an answer of `maxTokens`. */
export function promptBudget(contextTokens: number, maxTokens?: number): number {
  const reserve = Math.min(maxTokens ?? DEFAULT_ANSWER_RESERVE, Math.floor(contextTokens / 2));
  return Math.max(1_024, Math.floor((contextTokens - reserve) * TRIGGER_RATIO));
}

/** Window size: CHAT_CONTEXT_TOKENS when set to a sane number, else the node default. */
export function contextTokensFromEnv(env: Record<string, string | undefined> = process.env): number {
  const n = Number(env.CHAT_CONTEXT_TOKENS);
  return Number.isFinite(n) && n >= 2_048 && n <= 1_048_576 ? Math.floor(n) : DEFAULT_CONTEXT_TOKENS;
}

const RESULT_RE = /(<tool_result\b[^>]*>\n?|<tool_response>\n?)([\s\S]*?)(\n?<\/tool_result>|\n?<\/tool_response>)/g;

/** Clip every tool-result body in `text` longer than head + tail. Returns the text and how many were clipped. */
export function clipToolResults(text: string): { text: string; clipped: number } {
  let clipped = 0;
  const out = text.replace(RESULT_RE, (all, open: string, body: string, close: string) => {
    if (body.length <= CLIP_HEAD + CLIP_TAIL + 200) return all;
    clipped++;
    const cut = body.length - CLIP_HEAD - CLIP_TAIL;
    return `${open}${body.slice(0, CLIP_HEAD)}\n[… ${cut} characters of this earlier tool result clipped by c0mpute to fit the context window …]\n${body.slice(-CLIP_TAIL)}${close}`;
  });
  return { text: out, clipped };
}

function digestLine(m: ChatMessage): string | null {
  if (m.role !== "user" || /^\s*<tool_(result|response)\b/.test(m.content)) return null;
  const one = m.content.replace(/\s+/g, " ").trim();
  if (!one) return null;
  return `- user: ${one.length > DIGEST_LINE ? `${one.slice(0, DIGEST_LINE)}…` : one}`;
}

function digest(dropped: ChatMessage[]): string {
  const lines: string[] = [];
  let size = 0;
  for (const m of dropped) {
    const l = digestLine(m);
    if (!l) continue;
    if (size + l.length > DIGEST_MAX) {
      lines.push("- …");
      break;
    }
    lines.push(l);
    size += l.length + 1;
  }
  const head =
    `[Context note: ${dropped.length} earlier message${dropped.length === 1 ? "" : "s"} of this conversation ` +
    "were removed by c0mpute to fit the model's context window." +
    (lines.length ? " What the user asked in them:" : "") +
    "]";
  return [head, ...lines].join("\n");
}

/**
 * Fit `messages` (system prompt already in place) into `budget` prompt tokens.
 * Returns the same array object when nothing had to change.
 */
export function fitContext(messages: ChatMessage[], budget: number): { messages: ChatMessage[]; report: ContextReport } {
  const before = estimateTokens(messages);
  const report: ContextReport = { compacted: false, before, after: before, budget, clippedResults: 0, droppedMessages: 0 };
  if (before <= budget) return { messages, report };

  // 1. Clip tool results in all but the newest messages.
  let msgs = messages.map((m, i) => {
    if (i >= messages.length - KEEP_TOOL_TAIL || m.role === "system") return m;
    const c = clipToolResults(m.content);
    report.clippedResults += c.clipped;
    return c.clipped ? { ...m, content: c.text } : m;
  });

  // 2. Drop the oldest middle turns until it fits. Protected: the leading
  // system turns, the first user turn, and the newest message.
  if (estimateTokens(msgs) > budget) {
    let lead = 0;
    while (lead < msgs.length && msgs[lead].role === "system") lead++;
    const firstUser = msgs.findIndex((m, i) => i >= lead && m.role === "user");
    const keepHead = firstUser >= 0 && firstUser < msgs.length - 1 ? firstUser + 1 : lead;
    const head = msgs.slice(0, keepHead);
    let tail = msgs.slice(keepHead);
    const dropped: ChatMessage[] = [];
    const fits = () => estimateTokens([...head, ...tail]) + Math.ceil((DIGEST_MAX + 400) / 4) <= budget;
    while (tail.length > 1 && !fits()) dropped.push(tail.shift() as ChatMessage);
    // Start the kept tail on a user turn when one is left to start on, so
    // no reply is kept without the question it answered.
    while (tail.length > 1 && tail[0].role !== "user") dropped.push(tail.shift() as ChatMessage);
    if (dropped.length) {
      report.droppedMessages = dropped.length;
      const note = digest(dropped);
      const sys = head.findIndex((m) => m.role === "system");
      if (sys >= 0) head[sys] = { ...head[sys], content: `${head[sys].content}\n\n${note}` };
      else head.unshift({ role: "system", content: note });
    }
    msgs = [...head, ...tail];
  }

  report.compacted = report.clippedResults > 0 || report.droppedMessages > 0;
  report.after = estimateTokens(msgs);
  return { messages: report.compacted ? msgs : messages, report };
}
