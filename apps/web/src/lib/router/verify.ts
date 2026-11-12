// Deterministic checks on an answer. Free, instant and never wrong about what
// they test, so they run before (and often instead of) an LLM judge: a
// truncated answer, invalid JSON, a repetition loop or a stub reply fails
// here without spending a model call.

import type { Features, Verification } from "./types";

const STUB = /This is a stub response from the provider daemon|^ollama HTTP \d{3}:|^no (live node|Infernet node)/;
const REFUSAL = /^(I'?m sorry|I apologi[sz]e|I can(no|')t (help|assist|do)|As an AI( language model)?, I (can(no|')t|am unable))/i;

/** The JSON value in an answer: a fenced ```json block, or the whole text. */
export function extractJson(text: string): unknown {
  const fenced = text.match(/```(?:json)?\s*([\s\S]*?)```/i);
  const raw = (fenced ? fenced[1] : text).trim();
  try {
    return JSON.parse(raw);
  } catch {
    // A JSON object or array embedded in prose.
    const start = raw.search(/[[{]/);
    const end = Math.max(raw.lastIndexOf("}"), raw.lastIndexOf("]"));
    if (start >= 0 && end > start) return JSON.parse(raw.slice(start, end + 1));
    throw new Error("no JSON found");
  }
}

function balanced(code: string): boolean {
  const pairs: Record<string, string> = { ")": "(", "]": "[", "}": "{" };
  const stack: string[] = [];
  // Strip strings and comments roughly so brackets inside them don't count.
  const s = code
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/(^|[^:])\/\/.*$/gm, "$1")
    .replace(/#.*$/gm, "")
    .replace(/"(?:\\.|[^"\\\n])*"|'(?:\\.|[^'\\\n])*'|`(?:\\.|[^`\\])*`/g, "");
  for (const ch of s) {
    if (ch === "(" || ch === "[" || ch === "{") stack.push(ch);
    else if (ch in pairs) {
      if (stack.pop() !== pairs[ch]) return false;
    }
  }
  return stack.length === 0;
}

/** A line or phrase repeated over and over: small models degenerate this way. */
export function looping(text: string): boolean {
  const lines = text.split("\n").map((l) => l.trim()).filter((l) => l.length > 8);
  const counts = new Map<string, number>();
  for (const l of lines) counts.set(l, (counts.get(l) ?? 0) + 1);
  if ([...counts.values()].some((n) => n >= 5)) return true;
  // The same 40-char chunk 4+ times back to back.
  const m = text.match(/(.{40,}?)\1{3,}/s);
  return !!m;
}

export function verify(f: Features, text: string, finishReason?: string): Verification {
  const issues: string[] = [];
  const t = text.trim();
  let score = 1;
  const fail = (issue: string, weight = 1) => {
    issues.push(issue);
    score -= weight;
  };

  if (!t) fail("empty answer");
  if (STUB.test(t)) fail("stub/engine error text");
  if (finishReason === "length") fail("cut off at the token limit");
  if (t && looping(t)) fail("repetition loop");
  if (t && REFUSAL.test(t) && f.category !== "chat") fail("refused the task", 0.6);
  if (t && f.difficulty > 0.3 && f.category !== "chat" && t.length < 40) fail("too short for the task", 0.6);

  if (f.wantsJson && t) {
    try {
      extractJson(t);
    } catch {
      fail("invalid JSON");
    }
  }

  if (f.category === "code" && t) {
    const fences = (t.match(/```/g) ?? []).length;
    if (fences % 2 === 1) fail("unclosed code block");
    const blocks = [...t.matchAll(/```([\w+-]*)\n([\s\S]*?)```/g)];
    for (const [, lang, code] of blocks) {
      if (code.length > 20 && !balanced(code)) {
        fail(`unbalanced brackets in ${lang || "code"} block`, 0.7);
        break;
      }
      if (/^(js|javascript|mjs|cjs)$/i.test(lang)) {
        try {
          // Parses only; the code is never run.
          new Function(code);
        } catch (e) {
          const msg = e instanceof Error ? e.message : String(e);
          // Module syntax and top-level await are fine in real files.
          if (!/import|export|await|Cannot use/i.test(msg)) fail(`JavaScript syntax error: ${msg}`, 0.7);
        }
      }
    }
  }

  if (f.nonLatin && t.length > 40) {
    const letters = t.replace(/[\s\d\p{P}\p{S}]/gu, "").replace(/```[\s\S]*?```/g, "");
    const latin = (letters.match(/\p{Script=Latin}/gu) ?? []).length;
    if (letters.length > 20 && latin / letters.length > 0.85 && f.category !== "translate") fail("answered in a different language", 0.5);
  }

  score = Math.max(0, Math.round(score * 100) / 100);
  return { pass: issues.length === 0 || score >= 0.5 && !issues.some((i) => /empty|stub|cut off|loop|invalid JSON|unclosed/.test(i)), score, issues };
}
