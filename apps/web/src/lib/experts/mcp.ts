// MCP tools for the Experts store: list_experts, ask_expert, experts_status.
// Thin over the same catalog, gate and failover the API uses.

import type { ChatMessage, ChatRequest } from "../chat/completions";
import { parseChatRequest } from "../chat/completions";
import type { ExpertManifest } from "./manifest";
import { ExpertsError, type Asker } from "./service";

export type ExpertsMcpDeps = {
  list: () => ExpertManifest[];
  price: (m: ExpertManifest) => number;
  ask: (user: Asker, slug: string, req: ChatRequest, signal?: AbortSignal) => Promise<{ text: string; model: string; expert: ExpertManifest }>;
  status: (user: Asker) => Promise<unknown>;
  startTrial: (user: Asker) => Promise<unknown>;
};

export const EXPERTS_TOOLS = [
  {
    name: "list_experts",
    description:
      "The c0mpute Experts: first-party AI experts (security analyst, sales coach, automation engineer, legal info, tax info) running on the c0mpute network. Returns each expert's slug, what it does, example questions and price. 90-day free trial per CoinPay account, then a monthly subscription.",
    inputSchema: { type: "object", properties: {} },
    annotations: { title: "List experts", readOnlyHint: true, openWorldHint: false },
  },
  {
    name: "ask_expert",
    description:
      "Ask one c0mpute Expert a question (slug from list_experts). The first question starts this account's free trial (one per CoinPay account). Legal and tax experts give general information, not advice: pass their disclaimer on to the person.",
    inputSchema: {
      type: "object",
      properties: {
        expert: { type: "string", description: "Expert slug, e.g. security-analyst." },
        question: { type: "string", description: "The question." },
        messages: {
          type: "array",
          description: "Or a whole conversation: [{role: user|assistant, content}].",
          items: { type: "object", properties: { role: { type: "string", enum: ["user", "assistant"] }, content: { type: "string" } }, required: ["role", "content"] },
        },
      },
      required: ["expert"],
    },
    annotations: { title: "Ask an expert", readOnlyHint: true, openWorldHint: true },
  },
  {
    name: "experts_status",
    description: "This account's Experts trial (start, end, days left), subscriptions and prices. Pass start_trial: true to start the free trial now.",
    inputSchema: { type: "object", properties: { start_trial: { type: "boolean" } } },
    annotations: { title: "Experts trial and subscriptions", readOnlyHint: false, openWorldHint: false },
  },
];
export const EXPERTS_TOOL_NAMES = new Set(EXPERTS_TOOLS.map((t) => t.name));

export async function expertsTool(name: string, args: Record<string, unknown>, user: Asker, d: ExpertsMcpDeps, signal?: AbortSignal): Promise<{ text: string; isError?: boolean }> {
  try {
    if (name === "list_experts") {
      return {
        text: JSON.stringify(
          d.list().map((m) => ({
            slug: m.slug,
            name: m.name,
            category: m.category,
            summary: m.summary,
            example_questions: m.example_questions,
            price_usd_month: d.price(m),
            disclaimer: m.disclaimer ?? null,
            url: `https://c0mpute.com/experts/${m.slug}`,
          })),
        ),
      };
    }
    if (name === "experts_status") {
      if (args.start_trial === true) await d.startTrial(user);
      return { text: JSON.stringify(await d.status(user)) };
    }
    if (name === "ask_expert") {
      const slug = typeof args.expert === "string" ? args.expert.trim().replace(/^expert:/, "") : "";
      if (!slug) return { text: "ask_expert needs `expert` (a slug from list_experts)", isError: true };
      let messages: ChatMessage[] = [];
      if (Array.isArray(args.messages) && args.messages.length) messages = args.messages as ChatMessage[];
      else if (typeof args.question === "string" && args.question.trim()) messages = [{ role: "user", content: args.question }];
      else return { text: "ask_expert needs `question` or `messages`", isError: true };
      const req = parseChatRequest({ messages, stream: false });
      if (typeof req === "string") return { text: req, isError: true };
      const r = await d.ask(user, slug, req, signal);
      return { text: r.expert.disclaimer ? `${r.text}\n\n---\n${r.expert.disclaimer}` : r.text };
    }
    return { text: `unknown tool: ${name}`, isError: true };
  } catch (e) {
    if (e instanceof ExpertsError) return { text: JSON.stringify({ error: e.message, reason: e.code, ...e.extra }), isError: true };
    return { text: e instanceof Error ? e.message : String(e), isError: true };
  }
}
