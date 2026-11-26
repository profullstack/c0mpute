import { describe, expect, test } from "bun:test";
import type { ChatMessage } from "./completions";
import { wireMessages, C0_SYSTEM_PROMPT } from "./completions";
import {
  DEFAULT_CONTEXT_TOKENS,
  clipToolResults,
  contextTokensFromEnv,
  estimateTokens,
  fitContext,
  promptBudget,
} from "./context-budget";

const sys: ChatMessage = { role: "system", content: "You are helpful." };
const u = (content: string): ChatMessage => ({ role: "user", content });
const a = (content: string): ChatMessage => ({ role: "assistant", content });
const big = (n: number, ch = "x") => ch.repeat(n);
const pluginResult = (body: string) => `<tool_result name="mercury__accounts" status="ok">\n${body}\n</tool_result>`;
const clientResult = (body: string) => `<tool_response>\n${body}\n</tool_response>`;

describe("budget", () => {
  test("80% of the window left after the answer", () => {
    expect(promptBudget(32_768, 2_048)).toBe(Math.floor((32_768 - 2_048) * 0.8));
    // An absurd max_tokens never takes more than half the window.
    expect(promptBudget(16_384, 1_000_000)).toBe(Math.floor(8_192 * 0.8));
  });

  test("CHAT_CONTEXT_TOKENS overrides only with a sane number", () => {
    expect(contextTokensFromEnv({})).toBe(DEFAULT_CONTEXT_TOKENS);
    expect(contextTokensFromEnv({ CHAT_CONTEXT_TOKENS: "16384" })).toBe(16_384);
    expect(contextTokensFromEnv({ CHAT_CONTEXT_TOKENS: "12" })).toBe(DEFAULT_CONTEXT_TOKENS);
    expect(contextTokensFromEnv({ CHAT_CONTEXT_TOKENS: "lots" })).toBe(DEFAULT_CONTEXT_TOKENS);
  });
});

describe("fitContext", () => {
  test("a prompt that fits is returned untouched (same array)", () => {
    const msgs = [sys, u("hi"), a("hello"), u("how are you?")];
    const r = fitContext(msgs, 10_000);
    expect(r.messages).toBe(msgs);
    expect(r.report.compacted).toBe(false);
  });

  test("clips old tool results before dropping anything", () => {
    const msgs = [sys, u("check my balance"), a("```tool_call\n{}\n```"), u(pluginResult(big(40_000))), a("You have $5."), u("thanks, and the other account?")];
    const r = fitContext(msgs, 4_000);
    expect(r.report.compacted).toBe(true);
    expect(r.report.clippedResults).toBe(1);
    expect(r.report.droppedMessages).toBe(0);
    expect(r.messages).toHaveLength(msgs.length);
    expect(r.messages[3].content).toContain("clipped by c0mpute");
    expect(r.messages[3].content.startsWith('<tool_result name="mercury__accounts" status="ok">')).toBe(true);
    expect(r.messages[3].content.endsWith("</tool_result>")).toBe(true);
    expect(r.report.after).toBeLessThanOrEqual(4_000);
  });

  test("never clips the newest tool result the model is about to read", () => {
    const msgs = [sys, u("search"), a("```tool_call\n{}\n```"), u(clientResult(big(20_000)))];
    const r = fitContext(msgs, 2_000);
    expect(r.messages[r.messages.length - 1].content).toBe(msgs[3].content);
  });

  test("drops the oldest middle turns, keeps system, first user turn and newest, with a digest", () => {
    const msgs: ChatMessage[] = [sys, u("TASK: plan my trip to Lisbon")];
    for (let i = 0; i < 30; i++) msgs.push(u(`question ${i} ${big(2_000)}`), a(`answer ${i} ${big(2_000)}`));
    msgs.push(u("so what is the final plan?"));
    const budget = 6_000;
    const r = fitContext(msgs, budget);
    expect(r.report.droppedMessages).toBeGreaterThan(0);
    expect(r.report.after).toBeLessThanOrEqual(budget);
    expect(estimateTokens(r.messages)).toBeLessThanOrEqual(budget);
    expect(r.messages[0].role).toBe("system");
    expect(r.messages[0].content.startsWith("You are helpful.")).toBe(true);
    expect(r.messages[0].content).toContain("earlier messages of this conversation were removed by c0mpute");
    expect(r.messages[0].content).toContain("- user: question 0");
    expect(r.messages[1]).toEqual(msgs[1]);
    expect(r.messages[r.messages.length - 1]).toEqual(msgs[msgs.length - 1]);
    // The kept tail starts on a user turn: no orphaned reply.
    expect(r.messages[2].role).toBe("user");
  });

  test("tool-result turns are not quoted in the digest", () => {
    const msgs: ChatMessage[] = [sys, u("task")];
    for (let i = 0; i < 10; i++) msgs.push(a("```tool_call\n{}\n```"), u(clientResult(`r${i} ${big(3_000)}`)));
    msgs.push(u("done?"));
    const r = fitContext(msgs, 3_000);
    expect(r.report.droppedMessages).toBeGreaterThan(0);
    expect(r.messages[0].content).not.toContain("tool_response");
  });

  test("leaves a single oversized newest message alone (nothing else to drop)", () => {
    const msgs = [sys, u(big(100_000))];
    const r = fitContext(msgs, 2_000);
    expect(r.messages[1]).toEqual(msgs[1]);
  });
});

describe("clipToolResults", () => {
  test("short results stay whole; long ones keep head and tail", () => {
    expect(clipToolResults(pluginResult("small")).clipped).toBe(0);
    const body = `HEAD${big(10_000)}TAIL`;
    const c = clipToolResults(`${clientResult(body)}\n${pluginResult(body)}`);
    expect(c.clipped).toBe(2);
    expect(c.text).toContain("HEAD");
    expect(c.text).toContain("TAIL");
    expect(c.text.length).toBeLessThan(5_000);
  });
});

describe("wireMessages", () => {
  test("adds the house prompt and leaves a short chat alone", () => {
    const out = wireMessages({ messages: [u("hi")] }, 32_768);
    expect(out).toEqual([{ role: "system", content: C0_SYSTEM_PROMPT }, u("hi")]);
  });

  test("fits a long chat to a small window", () => {
    const messages: ChatMessage[] = [];
    for (let i = 0; i < 40; i++) messages.push(u(`q${i} ${big(3_000)}`), a(`a${i} ${big(3_000)}`));
    messages.push(u("last"));
    const out = wireMessages({ messages, max_tokens: 1_024 }, 16_384);
    expect(estimateTokens(out)).toBeLessThanOrEqual(promptBudget(16_384, 1_024));
    expect(out[0].content.startsWith(C0_SYSTEM_PROMPT)).toBe(true);
    expect(out[out.length - 1]).toEqual(u("last"));
  });
});
