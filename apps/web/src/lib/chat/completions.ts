// OpenAI-shaped chat on the c0mpute network. Every request runs on an Infernet
// provider node (p2p_only: never the paid hosted fallback) under c0mpute's
// trusted client key; c0mpute users are rate-limited per account here.

export type ChatMessage = { role: "system" | "user" | "assistant"; content: string };
export type ChatRequest = {
  model?: string;
  messages: ChatMessage[];
  stream?: boolean;
  temperature?: number;
  max_tokens?: number;
};

const ROLES = new Set(["system", "user", "assistant"]);
const MAX_CHARS = 200_000;
const MAX_MESSAGES = 200;

/** Validate and strip a client body down to what we forward. */
export function parseChatRequest(body: unknown): ChatRequest | string {
  if (!body || typeof body !== "object") return "body must be a JSON object";
  const b = body as Record<string, unknown>;
  if (!Array.isArray(b.messages) || b.messages.length === 0) return "messages[] is required";
  if (b.messages.length > MAX_MESSAGES) return `at most ${MAX_MESSAGES} messages`;
  let chars = 0;
  const messages: ChatMessage[] = [];
  for (const m of b.messages as Record<string, unknown>[]) {
    if (!m || typeof m.content !== "string" || !ROLES.has(m.role as string)) {
      return "each message must be { role: system|user|assistant, content: string }";
    }
    chars += m.content.length;
    messages.push({ role: m.role as ChatMessage["role"], content: m.content });
  }
  if (chars > MAX_CHARS) return `messages total more than ${MAX_CHARS} characters`;
  const out: ChatRequest = { messages, stream: b.stream === true };
  if (typeof b.model === "string" && b.model && b.model !== "auto") out.model = b.model.slice(0, 200);
  if (typeof b.temperature === "number" && Number.isFinite(b.temperature)) {
    out.temperature = Math.min(2, Math.max(0, b.temperature));
  }
  if (typeof b.max_tokens === "number" && Number.isFinite(b.max_tokens)) {
    out.max_tokens = Math.min(8192, Math.max(1, Math.floor(b.max_tokens)));
  }
  return out;
}

export type Upstream = (path: string, init?: RequestInit) => Promise<Response>;

/** Forward to Infernet; the Response (JSON or SSE) is handed back as-is. */
export async function forwardChat(upstream: Upstream, req: ChatRequest, signal?: AbortSignal) {
  return upstream("/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json", accept: req.stream ? "text/event-stream" : "application/json" },
    body: JSON.stringify({ ...req, p2p_only: true }),
    signal,
  });
}

/** Non-streaming convenience for MCP and server callers: the answer text. */
export async function chatText(upstream: Upstream, req: ChatRequest): Promise<{ text: string; model: string }> {
  const res = await forwardChat(upstream, { ...req, stream: false }, AbortSignal.timeout(300_000));
  const body = (await res.json().catch(() => ({}))) as {
    model?: string;
    choices?: { message?: { content?: string } }[];
    error?: { message?: string };
  };
  if (!res.ok) throw new Error(body.error?.message || `inference failed (HTTP ${res.status})`);
  return { text: body.choices?.[0]?.message?.content ?? "", model: body.model ?? req.model ?? "auto" };
}

export async function listModels(upstream: Upstream): Promise<string[]> {
  const res = await upstream("/v1/models", { signal: AbortSignal.timeout(15_000) });
  if (!res.ok) throw new Error(`models HTTP ${res.status}`);
  const body = (await res.json()) as { data?: { id: string }[] };
  return (body.data ?? []).map((m) => m.id);
}
