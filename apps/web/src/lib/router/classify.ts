// Task classification: what kind of work a request is and how hard.
//
// Deterministic and cheap (regexes and counts, no model call), so it adds no
// latency, costs nothing, and every decision can be explained: `reasons`
// says which signals fired.

import type { Category, Features, Msg } from "./types";

/** ~4 characters per token for Latin text; fine for budgeting. */
export function estimateTokens(text: string): number {
  return Math.ceil(text.length / 4);
}

type Signal = { category: Category; re: RegExp; weight: number; label: string };

const SIGNALS: Signal[] = [
  // code
  { category: "code", re: /```|^\s{4}\S.*[;{}]\s*$/m, weight: 3, label: "code block" },
  { category: "code", re: /\b(function|def|class|import|const|let|fn|impl|struct|async|await|return|=>|SELECT|INSERT|UPDATE)\b/, weight: 1.5, label: "code keywords" },
  { category: "code", re: /\b(bug|debug|refactor|implement|compile[sd]?|stack ?trace|traceback|segfault|regex|unit tests?|typescript|javascript|python|rust|golang|java|c\+\+|sql|bash|shell script|dockerfile|kubernetes|api endpoint|null pointer|exception)\b/i, weight: 2, label: "programming terms" },
  { category: "code", re: /\b(error|warning)\[?E?\d*\]?:|Traceback \(most recent call last\)|at [\w.$]+ \(.*:\d+:\d+\)/, weight: 3, label: "error output" },
  { category: "code", re: /\b(write|fix|build|make) (me )?(a |an |the )?(function|script|program|class|component|cli|endpoint|query)\b/i, weight: 3, label: "asks for code" },
  // math
  { category: "math", re: /\b(solve|integral|derivative|equation|prove|theorem|probability|calculate|compute|factorial|matrix|eigen\w*|limit of|sum of)\b/i, weight: 2, label: "math terms" },
  { category: "math", re: /\$[^$]+\$|\\frac|\\int|\\sum|\d+\s*[\^*/]\s*\d+|\d+\s*[+-]\s*\d+\s*=|\bx\s*=|\bf\(x\)/, weight: 2, label: "formula" },
  { category: "math", re: /\d+(\.\d+)?\s*%\s*of\s*[\d$]|\b(how much is|what'?s|what is)\s+[\d$(]|\b\d[\d,.]*\s*(times|divided by|plus|minus)\s*\d/i, weight: 2, label: "arithmetic" },
  // reasoning
  { category: "reasoning", re: /\b(step by step|think through|why does|why is|explain why|reason about|trade-?offs?|pros and cons|compare|which is better|plan (out|for)|riddle|puzzle|logic)\b/i, weight: 1.5, label: "reasoning cues" },
  // extraction / structured output
  { category: "extract", re: /\b(json|yaml|csv|schema|extract|parse|key-value|fields?|structured)\b/i, weight: 2, label: "structured output" },
  // summarize
  { category: "summarize", re: /\b(summari[sz]e|summary|tl;?dr|key points|in (a few|\d+) (sentences|bullets)|condense)\b/i, weight: 3, label: "summarize" },
  // translate
  { category: "translate", re: /\b(translate|translation|in (spanish|french|german|italian|portuguese|chinese|japanese|korean|russian|arabic|hindi|dutch)|to (spanish|french|german|english|chinese|japanese))\b/i, weight: 3.5, label: "translate" },
  // writing
  { category: "writing", re: /\b(write|draft|compose|rewrite|polish|proofread)\b.{0,40}\b(essay|poem|story|blog|post|article|email|letter|cover letter|tweet|caption|copy|speech|lyrics|bio)\b/i, weight: 3, label: "writing task" },
  { category: "writing", re: /\b(tone|more formal|less formal|make it (shorter|longer|punchier))\b/i, weight: 1.5, label: "style edit" },
  // domains with house specialists
  { category: "legal", re: /\b(contract|lawsuit|sue|liabilit\w+|attorney|lawyer|legal(ly)?|terms of service|nda|tenant|landlord|copyright|trademark|patent|gdpr|ccpa|court|subpoena|indemnif\w+)\b/i, weight: 2.5, label: "legal terms" },
  { category: "tax", re: /\b(tax(es)?|irs|ftb|deduct\w*|1099|w-?2|w-?9|schedule c|llc tax|s-?corp|vat|sales tax|withholding|depreciation|capital gains|estimated payments?)\b/i, weight: 2.5, label: "tax terms" },
  { category: "business", re: /\b(startup|saas|go-to-market|gtm|pricing (strategy|page|tiers?)|churn|mrr|arr|cac|ltv|business plan|market fit|fundrais\w+|investor|pitch deck|positioning|competitor)\b/i, weight: 2.5, label: "business terms" },
];

const GREETING = /^\s*(hi|hey|hello|yo|sup|thanks|thank you|ok|okay|cool|gm|good (morning|evening|night)|we on\??|test)\b[\s!.?]*$/i;
const HARD = /\b(prove|optimi[sz]e|architecture|concurren\w+|race condition|distributed|scal(e|ing|ability)|security (review|audit)|edge cases?|complexity|formal(ly)?|derive|multi-?step|end-to-end|production)\b/i;
const CONSTRAINT = /\b(must|should|ensure|exactly|at least|at most|without|only|never|always|don't|do not)\b/gi;

function lastUser(messages: Msg[]): string {
  for (let i = messages.length - 1; i >= 0; i--) if (messages[i].role === "user") return messages[i].content;
  return "";
}

export function classify(messages: Msg[], opts: { responseFormatJson?: boolean } = {}): Features {
  const ask = lastUser(messages);
  const system = messages.filter((m) => m.role === "system").map((m) => m.content).join("\n");
  const all = messages.map((m) => m.content).join("\n");
  const promptTokens = estimateTokens(all);
  const reasons: string[] = [];

  const scores = new Map<Category, number>();
  const labels = new Map<Category, string[]>();
  for (const s of SIGNALS) {
    if (s.re.test(ask)) {
      scores.set(s.category, (scores.get(s.category) ?? 0) + s.weight);
      labels.set(s.category, [...(labels.get(s.category) ?? []), s.label]);
    } else if (system && s.re.test(system)) {
      // The system prompt counts, at a third of the weight.
      scores.set(s.category, (scores.get(s.category) ?? 0) + s.weight / 3);
    }
  }
  // A long pasted document with a short question is a summarize/QA job.
  if (promptTokens > 3000 && estimateTokens(ask) < 200 && !scores.has("code")) {
    scores.set("summarize", (scores.get("summarize") ?? 0) + 2);
    labels.set("summarize", [...(labels.get("summarize") ?? []), "long document"]);
  }

  const ranked = [...scores.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  let category: Category = "chat";
  let runnerUp: Features["runnerUp"];
  const total = ranked.reduce((n, [, v]) => n + v, 0);
  if (ranked.length && ranked[0][1] >= 1.5 && !GREETING.test(ask)) {
    category = ranked[0][0];
    reasons.push(`${category}: ${(labels.get(category) ?? ["system prompt"]).join(", ")}`);
    if (ranked[1]) runnerUp = { category: ranked[1][0], share: Math.round((ranked[1][1] / total) * 100) / 100 };
  } else {
    reasons.push(GREETING.test(ask) ? "chat: greeting/small talk" : "chat: no strong task signal");
  }

  const wantsJson = !!opts.responseFormatJson || /\b(json|as an object|valid json)\b/i.test(ask);
  if (wantsJson) reasons.push("output must be JSON");

  // Difficulty: length, constraints, hard-topic words, code size, multi-part asks.
  const askTokens = estimateTokens(ask);
  let d = Math.min(0.35, Math.log10(1 + askTokens) / 8);
  const constraints = (ask.match(CONSTRAINT) ?? []).length;
  d += Math.min(0.2, constraints * 0.04);
  if (HARD.test(ask)) {
    d += 0.25;
    reasons.push("hard-topic words");
  }
  const parts = (ask.match(/^\s*(\d+[.)]|[-*•])\s+/gm) ?? []).length;
  if (parts >= 3) {
    d += Math.min(0.15, parts * 0.03);
    reasons.push(`${parts}-part request`);
  }
  if (category === "code" && /```[\s\S]{800,}```/.test(ask)) d += 0.1;
  if (category === "math" || category === "reasoning") d += 0.1;
  if (GREETING.test(ask)) d = 0.02;
  const difficulty = Math.round(Math.max(0, Math.min(1, d)) * 100) / 100;
  reasons.push(`difficulty ${difficulty}`);

  const letters = ask.replace(/[\s\d\p{P}\p{S}]/gu, "");
  const nonLatinChars = letters.replace(/[\p{Script=Latin}]/gu, "").length;
  const nonLatin = letters.length > 10 && nonLatinChars / letters.length > 0.5;
  if (nonLatin) reasons.push("non-Latin script");

  return { category, runnerUp, difficulty, promptTokens, wantsJson, nonLatin, reasons };
}
