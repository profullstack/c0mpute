// Conversation → node affinity for chat on the c0mpute network.
//
// Clients resend the whole conversation every turn. A node that served the
// previous turn still holds that prompt in its engine's cache (llama-server
// keeps a slot's prompt and reuses the matching prefix), so sending the next
// turn back to it skips re-reading the history: on a long chat that is most of
// the turn's time. Without this, the auction's rotation sent consecutive turns
// to different nodes on purpose.
//
// The worker that answered comes back from Infernet as `x-infernet-worker`
// (JSON answers) or an SSE comment `: worker {"worker":"…"}` (streams). It is
// remembered here per conversation and sent with the next turn as
// `prefer_worker`; Infernet passes it to the c0mpute jobs bridge as
// `prefer_peers`, which ranks that worker first when it bids and is not
// overloaded. A hint only: if the node is gone or busy, the auction runs as
// before, and nothing is refused.
//
// A conversation is named by the model and its first user message (what every
// later turn still starts with), hashed. In memory per server: a restart only
// costs one turn without a preference.

import { createHash } from "node:crypto";
import type { ChatMessage } from "./completions";

const TTL_MS = 60 * 60_000;
const MAX_ENTRIES = 50_000;
/** libp2p peer ids as base58 (Ed25519 "12D3Koo…", RSA "Qm…"). */
const PEER_RE = /^(12D3Koo|Qm)[1-9A-HJ-NP-Za-km-z]{30,60}$/;

const remembered = new Map<string, { worker: string; at: number }>();

/** The conversation key, or null for a conversation with no user message. */
export function conversationKey(model: string | undefined, messages: readonly ChatMessage[]): string | null {
  const first = messages.find((m) => m.role === "user");
  if (!first) return null;
  return createHash("sha256").update(`${model ?? "auto"}\n${first.content}`).digest("base64url").slice(0, 22);
}

export function isPeerId(v: unknown): v is string {
  return typeof v === "string" && PEER_RE.test(v);
}

/** The worker that served this conversation last, if recent. */
export function preferredWorker(key: string | null, now = Date.now()): string | null {
  if (!key) return null;
  const e = remembered.get(key);
  if (!e) return null;
  if (now - e.at > TTL_MS) {
    remembered.delete(key);
    return null;
  }
  return e.worker;
}

export function rememberWorker(key: string | null, worker: unknown, now = Date.now()) {
  if (!key || !isPeerId(worker)) return;
  remembered.delete(key);
  remembered.set(key, { worker, at: now });
  // Map keeps insertion order: the first keys are the least recently used.
  while (remembered.size > MAX_ENTRIES) {
    const oldest = remembered.keys().next().value;
    if (oldest === undefined) break;
    remembered.delete(oldest);
  }
}

/** Tests only. */
export function resetAffinity() {
  remembered.clear();
}

const WORKER_LINE = /^: worker (\{.*\})\s*$/m;

/**
 * Remember who answered `res`, leaving the response itself untouched: the
 * header for JSON, the `: worker` SSE comment for a stream (scanned as the
 * bytes pass through).
 */
export function captureWorker(res: Response, key: string | null): Response {
  if (!key) return res;
  const header = res.headers.get("x-infernet-worker");
  if (header) {
    rememberWorker(key, header);
    return res;
  }
  if (!res.body || !(res.headers.get("content-type") ?? "").includes("text/event-stream")) return res;
  const decoder = new TextDecoder();
  let tail = "";
  let found = false;
  const scan = new TransformStream<Uint8Array, Uint8Array>({
    transform(chunk, controller) {
      controller.enqueue(chunk);
      if (found) return;
      tail = (tail + decoder.decode(chunk, { stream: true })).slice(-4096);
      const m = WORKER_LINE.exec(tail);
      if (!m) return;
      try {
        rememberWorker(key, (JSON.parse(m[1]) as { worker?: unknown }).worker);
        found = true;
      } catch {
        // A partial line: the next chunk completes it.
      }
    },
  });
  return new Response(res.body.pipeThrough(scan), { status: res.status, statusText: res.statusText, headers: res.headers });
}
