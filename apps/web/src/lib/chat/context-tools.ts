// Model-driven context management for the chat plugin loop (arXiv 2611.34147,
// "Letting Models Manage Their Own Context", NVIDIA 2026).
//
// lib/chat/context-budget.ts trims a prompt for the model when it overflows
// (harness-driven). This lets the model do it first, with what only it knows:
// which earlier messages it still needs word for word. Two built-in tools ride
// with the person's plugins:
//
//   context__collapse({ from, to, summary })  replace a run of messages by a summary
//   context__recall({ tag })                  read an original (or a collapsed run) again
//
// Messages are labelled ⟦m:<tag>⟧, where the tag is a hash of the message, so
// a label stays the same across steps and turns (clients resend the whole
// history every turn). Nothing original is archived here: the originals are
// in every request, so recall reads them from it. Only the model's summaries
// are kept (sealed, per account, lib/chat/plugin-store.ts), and they are applied
// again on later turns wherever their first and last messages still appear.
//
// Labels, the usage line and the instruction appear only once the prompt is
// past OFFER_AT of its budget; the instruction says to compact past COMPACT_AT.
// Without a timing instruction models compact far too early (the paper's
// "context anxiety": Kimi at 29%, Opus at 16% of the window), and a collapse
// asked for below OFFER_AT is refused. At 100% context-budget.ts still trims.

import { createHash } from "node:crypto";
import type { Plugin } from "@profullstack/chat-plugins";
import { estimateTokens } from "./context-budget";

export const CONTEXT_SLUG = "context";
/** Labels and the usage line appear past this share of the prompt budget. */
export const OFFER_AT = 0.6;
/** The model is told to compact past this share. */
export const COMPACT_AT = 0.85;
const SUMMARY_MAX = 4_000;
const RECALL_MAX = 12_000;

export type Collapse = { from: string; to: string; summary: string };
type Msg = { role: string; content: string };
type ToolResult = { isError: boolean; content: { type: "text"; text: string }[]; text: string; modelText: string };

/** On unless CHAT_CONTEXT_TOOLS is 0/off/false/no. */
export function contextToolsEnabled(env: Record<string, string | undefined> = process.env): boolean {
  return !/^(0|off|false|no)$/i.test((env.CHAT_CONTEXT_TOOLS ?? "").trim());
}

/** A message's stable tag: 6 hex characters of sha256(role, content). */
export function tagOf(m: Msg): string {
  return createHash("sha256").update(`${m.role}\n${m.content}`).digest("hex").slice(0, 6);
}

const LABEL_RE = /⟦[ms]:[0-9a-f.]+⟧ ?/g;
/** Remove context labels a model copied into its answer. */
export const stripLabels = (t: string) => t.replace(LABEL_RE, "");

/** "⟦m:a1b2c3⟧", "m:a1b2c3", "a1b2c3", "a1b2c3..d4e5f6" → the hex tags it names. */
function tagsIn(raw: unknown): string[] {
  return typeof raw === "string" ? (raw.toLowerCase().match(/[0-9a-f]{6}/g) ?? []) : [];
}

export const contextPlugin: Plugin = {
  slug: CONTEXT_SLUG,
  name: "Context",
  url: "builtin:context",
  tools: [
    {
      name: "collapse",
      title: "Collapse earlier messages",
      description:
        "Free up context window: replace a run of earlier messages with your own summary. Only when messages carry ⟦m:tag⟧ labels and the usage line says the window is past 85%. from/to are the first and last tags of the run; the summary must keep every fact, number, name, id and decision you still need. Originals stay readable with context__recall.",
      inputSchema: {
        type: "object",
        properties: {
          from: { type: "string", description: "tag of the first message to collapse, e.g. a1b2c3" },
          to: { type: "string", description: "tag of the last message to collapse" },
          summary: { type: "string", description: "what to keep from those messages" },
        },
        required: ["from", "to", "summary"],
      },
      annotations: { readOnlyHint: true },
    },
    {
      name: "recall",
      title: "Recall a collapsed message",
      description: "Read an earlier message again, word for word, by its tag (or a collapsed run by its from..to tag).",
      inputSchema: {
        type: "object",
        properties: { tag: { type: "string", description: "e.g. a1b2c3, or a1b2c3..d4e5f6 for a collapsed run" } },
        required: ["tag"],
      },
      annotations: { readOnlyHint: true },
    },
  ],
};

type Span = { from: number; to: number; c: Collapse };

const ok = (text: string): ToolResult => ({ isError: false, content: [{ type: "text", text }], text, modelText: text });
const fail = (text: string): ToolResult => ({ isError: true, content: [{ type: "text", text }], text, modelText: text });

/**
 * One plugin turn's view of the context. `view` turns the loop's messages
 * into what the model sees; `call` runs the two tools against the messages of
 * the latest view.
 */
export class ContextSession {
  private collapses: Collapse[];
  private raw: Msg[] = [];
  private usage = 0;
  readonly made: Collapse[] = [];

  constructor(
    private readonly opts: {
      budget: number;
      saved?: Collapse[];
      persist?: (c: Collapse) => Promise<void>;
      model?: () => string;
    },
  ) {
    this.collapses = (opts.saved ?? []).slice();
  }

  /** Where each applicable collapse sits in `msgs`, in order, without overlaps. */
  private spans(msgs: Msg[]): Span[] {
    const tags = msgs.map(tagOf);
    const out: Span[] = [];
    for (const c of this.collapses) {
      const from = tags.findIndex((t, i) => t === c.from && msgs[i].role !== "system");
      if (from < 0) continue;
      const to = tags.findIndex((t, i) => i >= from && t === c.to);
      // Never the newest message: it is what the model is answering.
      if (to < 0 || to >= msgs.length - 1) continue;
      if (out.some((s) => from <= s.to && to >= s.from)) continue;
      out.push({ from, to, c });
    }
    return out.sort((a, b) => a.from - b.from);
  }

  private render(msgs: Msg[], labels: boolean): Msg[] {
    const spans = this.spans(msgs);
    const out: Msg[] = [];
    for (let i = 0; i < msgs.length; i++) {
      const s = spans.find((x) => x.from === i);
      if (s) {
        const label = labels ? `⟦s:${s.c.from}..${s.c.to}⟧ ` : "";
        out.push({
          role: "user",
          content: `${label}[Summary of ${s.to - s.from + 1} earlier messages, written by the assistant to save context]\n${s.c.summary}`,
        });
        i = s.to;
        continue;
      }
      const m = msgs[i];
      out.push(labels && m.role !== "system" ? { ...m, content: `⟦m:${tagOf(m)}⟧ ${m.content}` } : m);
    }
    return out;
  }

  /** The messages the model sees for `msgs` (system prompt first, as the loop builds it). */
  view(msgs: Msg[]): Msg[] {
    this.raw = msgs;
    const plain = this.render(msgs, false);
    this.usage = estimateTokens(plain) / this.opts.budget;
    if (this.usage < OFFER_AT) return plain;
    const out = this.render(msgs, true);
    const pct = Math.round(this.usage * 100);
    const note =
      `Context window: about ${pct}% of this conversation's budget is in use. Each message starts with a ⟦m:tag⟧ label (never repeat labels in your answers). ` +
      (this.usage >= COMPACT_AT
        ? "It is past 85%: before continuing, call context__collapse on a run of older messages you no longer need word for word (old tool results first), with a summary that keeps every fact, number, name and decision you still need. "
        : `When it passes ${Math.round(COMPACT_AT * 100)}%, use context__collapse on older messages you no longer need word for word; not before. `) +
      "Originals stay readable with context__recall. Never collapse the person's latest message. At 100% c0mpute removes the oldest messages for you.";
    const sys = out.findIndex((m) => m.role === "system");
    if (sys >= 0) out[sys] = { ...out[sys], content: `${out[sys].content}\n\n${note}` };
    else out.unshift({ role: "system", content: note });
    return out;
  }

  /** Run context__<tool> against the messages of the latest view. */
  async call(tool: string, args: Record<string, unknown>): Promise<ToolResult> {
    if (tool === "recall") return this.recall(args);
    if (tool === "collapse") return this.collapse(args);
    return fail(`Unknown context tool "${tool}". Use context__collapse or context__recall.`);
  }

  private recall(args: Record<string, unknown>): ToolResult {
    const want = tagsIn(args.tag);
    if (!want.length) return fail("Give the tag of a message, e.g. a1b2c3 (or a1b2c3..d4e5f6 for a collapsed run).");
    const tags = this.raw.map(tagOf);
    const from = tags.indexOf(want[0]);
    if (from < 0) return fail(`No message with tag ${want[0]} in this conversation.`);
    const to = want[1] ? tags.findIndex((t, i) => i >= from && t === want[1]) : from;
    if (to < 0) return fail(`No message with tag ${want[1]} after ${want[0]}.`);
    const text = this.raw
      .slice(from, to + 1)
      .map((m) => `[${m.role} ${tagOf(m)}]\n${m.content}`)
      .join("\n\n");
    return ok(text.length > RECALL_MAX ? `${text.slice(0, RECALL_MAX)}\n[… recall cut at ${RECALL_MAX} characters]` : text);
  }

  private async collapse(args: Record<string, unknown>): Promise<ToolResult> {
    const pct = Math.round(this.usage * 100);
    if (this.usage < OFFER_AT) return fail(`The context is only about ${pct}% full: nothing to collapse yet. Carry on with the task.`);
    // A collapsed run is named from..to: its first tag starts, its last tag ends.
    const a = tagsIn(args.from);
    const b = tagsIn(args.to);
    const fromTag = a[0];
    const toTag = b.length ? b[b.length - 1] : a[a.length - 1];
    const summary = typeof args.summary === "string" ? args.summary.trim() : "";
    if (!fromTag || !toTag) return fail("from and to must be message tags, e.g. a1b2c3.");
    if (!summary) return fail("Write a summary of what you still need from those messages.");
    if (summary.length > SUMMARY_MAX) return fail(`Keep the summary under ${SUMMARY_MAX} characters.`);
    const tags = this.raw.map(tagOf);
    const from = tags.findIndex((t, i) => t === fromTag && this.raw[i].role !== "system");
    if (from < 0) return fail(`No message with tag ${fromTag}. Use the ⟦m:tag⟧ labels as shown.`);
    const to = tags.findIndex((t, i) => i >= from && t === toTag);
    if (to < 0) return fail(`No message with tag ${toTag} at or after ${fromTag}.`);
    if (to >= this.raw.length - 1) return fail("The latest message cannot be collapsed; pick older ones.");
    const run = this.raw.slice(from, to + 1);
    const runChars = run.reduce((n, m) => n + m.content.length, 0);
    if (summary.length >= runChars) return fail("The summary is not shorter than the messages it replaces; collapse a longer run or write less.");

    const before = estimateTokens(this.render(this.raw, false));
    const c: Collapse = { from: fromTag, to: toTag, summary };
    // A new run that covers older collapses replaces them.
    const inside = this.spans(this.raw).filter((s) => s.from >= from && s.to <= to).map((s) => s.c);
    this.collapses = [...this.collapses.filter((x) => !inside.includes(x)), c];
    const after = estimateTokens(this.render(this.raw, false));
    this.usage = after / this.opts.budget;
    this.made.push(c);
    console.info(
      `chat context tool: collapse model=${this.opts.model?.() ?? "?"} messages=${run.length} tokens ${before} -> ${after} (${pct}% -> ${Math.round(this.usage * 100)}%)`,
    );
    await this.opts.persist?.(c).catch((e) => console.warn("chat context tool: could not save a collapse", e instanceof Error ? e.message : e));
    return ok(
      `Collapsed ${run.length} message${run.length === 1 ? "" : "s"} into your summary (about ${before - after} tokens freed; the context is now about ${Math.round(this.usage * 100)}% full). Continue with the person's request.`,
    );
  }
}
