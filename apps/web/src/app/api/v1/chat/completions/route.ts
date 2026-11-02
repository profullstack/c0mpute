import { forwardChat, parseChatRequest } from "@/lib/chat/completions";
import { currentUser, infernet, json, rateLimit, sameOrigin, unauthorized } from "@/lib/chat/server";

export const dynamic = "force-dynamic";
export const maxDuration = 300;

const err = (status: number, message: string, headers: Record<string, string> = {}) =>
  json({ error: { message, type: "c0mpute_error", code: status } }, status, headers);

/**
 * OpenAI-compatible chat completions on the c0mpute network.
 *   Authorization: Bearer c0k_...   (or the c0mpute.com session cookie)
 *   POST { model?, messages, stream?, temperature?, max_tokens? }
 */
export async function POST(req: Request) {
  const user = await currentUser(req);
  if (!user) return unauthorized();
  if (!sameOrigin(req)) return err(403, "cross-site request refused");
  const rl = rateLimit(user.id);
  if (!rl.ok) return err(429, "rate limit: too many chats this hour", { "retry-after": String(rl.retryAfter) });

  const parsed = parseChatRequest(await req.json().catch(() => null));
  if (typeof parsed === "string") return err(400, parsed);

  let upstream: Response;
  try {
    upstream = await forwardChat(infernet, parsed, req.signal);
  } catch (e) {
    return err(502, `network unreachable: ${e instanceof Error ? e.message : e}`);
  }
  // Pass Infernet's body straight through: JSON, OpenAI SSE chunks, or its error envelope.
  return new Response(upstream.body, {
    status: upstream.status,
    headers: {
      "content-type": upstream.headers.get("content-type") ?? "application/json",
      "cache-control": "no-store",
      "x-accel-buffering": "no",
    },
  });
}
