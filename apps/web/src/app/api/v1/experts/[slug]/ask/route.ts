import { parseChatRequest } from "@/lib/chat/completions";
import { currentUser, json, sameOrigin, unauthorized } from "@/lib/chat/server";
import { recordUsage } from "@/lib/leaderboard/server";
import { expertChatResponse } from "@/lib/experts/server";

export const dynamic = "force-dynamic";
export const maxDuration = 300;

/**
 * POST /api/v1/experts/<slug>/ask   { question } | { messages: [{role, content}] }, stream?
 * Authorization: Bearer c0k_... (or the c0mpute.com session). The first question
 * starts the account's free trial (one per CoinPay account); after it, 402 with
 * subscribe links until a plan is paid. Same answer shape as
 * /v1/chat/completions with model "expert:<slug>".
 */
export async function POST(req: Request, { params }: { params: Promise<{ slug: string }> }) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return json({ error: { message: "cross-site request refused", code: 403 } }, 403);
  const { slug } = await params;
  const raw = (await req.json().catch(() => null)) as Record<string, unknown> | null;
  if (!raw) return json({ error: { message: "body must be JSON: { question } or { messages }", code: 400 } }, 400);
  const messages = Array.isArray(raw.messages) && raw.messages.length ? raw.messages : typeof raw.question === "string" && raw.question.trim() ? [{ role: "user", content: raw.question }] : null;
  if (!messages) return json({ error: { message: "send { question: string } or { messages: [...] }", code: 400 } }, 400);
  const parsed = parseChatRequest({ messages, stream: raw.stream === true, max_tokens: raw.max_tokens });
  if (typeof parsed === "string") return json({ error: { message: parsed, code: 400 } }, 400);
  recordUsage(user.id, user.token_kind === "session" ? "chat" : "api", `expert:${slug}`);
  return expertChatResponse({ user, slug, req: parsed, signal: req.signal });
}
