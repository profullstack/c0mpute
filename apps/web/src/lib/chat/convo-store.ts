// Chat conversations synced to the account, so the web chat, the TUI and the
// CLI share history and a dropped connection or a new device loses nothing.
//
// One row per (user, id) in `chat_conversations`. Title and model are plain
// (the list needs them); the messages are sealed with AES-256-GCM, bound to
// their row. The key is C0MPUTE_CHAT_KEY, or one derived from
// C0MPUTE_PLUGIN_KEY so no new secret is needed. This is encryption at rest:
// the server can read a conversation (it has to send the prompts to a node
// anyway); a stolen database dump cannot.
//
// Sync is last writer wins on the client's `updated` clock. A delete leaves a
// tombstone so other devices drop the conversation on their next sync.

import { hkdfSync } from "node:crypto";
import { seal, unseal } from "./plugin-store";
import type { Row, Sql } from "./store";

export const CONVO_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS chat_conversations (
     user_id    TEXT NOT NULL,
     id         TEXT NOT NULL,
     title      TEXT NOT NULL,
     model      TEXT NOT NULL,
     messages   TEXT,
     updated    INTEGER NOT NULL,
     deleted    INTEGER NOT NULL DEFAULT 0,
     size       INTEGER NOT NULL DEFAULT 0,
     created_at TEXT NOT NULL,
     PRIMARY KEY (user_id, id)
   )`,
  `CREATE INDEX IF NOT EXISTS chat_conversations_user_updated ON chat_conversations (user_id, updated)`,
];

export const CONVO_ID = /^[A-Za-z0-9_-]{4,64}$/;
export const MAX_CONVOS = 500;
export const MAX_MESSAGES = 2000;
export const MAX_BYTES = 1024 * 1024;
const ROLES = new Set(["user", "assistant", "system", "note"]);

export type ConvoMsg = { role: string; content: string } & Record<string, unknown>;
export type Conversation = { id: string; title: string; model: string; messages: ConvoMsg[]; updated: number };
export type ConvoSummary = { id: string; title: string; model: string; updated: number; deleted: boolean };

export class ConvoError extends Error {
  constructor(
    message: string,
    public status = 400,
    public conversation?: Conversation,
  ) {
    super(message);
  }
}

/** The sealing key: C0MPUTE_CHAT_KEY (32 bytes) or HKDF of the plugin key. */
export function convoKey(chatKey: Buffer | null, pluginKey: Buffer | null): Buffer | null {
  if (chatKey) return chatKey;
  if (!pluginKey) return null;
  return Buffer.from(hkdfSync("sha256", pluginKey, Buffer.alloc(0), "c0mpute chat conversations v1", 32));
}

/** Validate and normalise a PUT body; throws ConvoError with what is wrong. */
export function parseConversation(id: string, body: unknown): Conversation {
  if (!CONVO_ID.test(id)) throw new ConvoError("conversation id must be 4-64 of A-Z a-z 0-9 _ -");
  if (!body || typeof body !== "object") throw new ConvoError("body must be a JSON object");
  const b = body as Record<string, unknown>;
  if (!Array.isArray(b.messages)) throw new ConvoError("messages must be an array");
  if (b.messages.length > MAX_MESSAGES) throw new ConvoError(`at most ${MAX_MESSAGES} messages per conversation`, 413);
  const messages: ConvoMsg[] = [];
  for (const m of b.messages) {
    if (!m || typeof m !== "object") throw new ConvoError("each message must be an object");
    const r = m as Record<string, unknown>;
    if (typeof r.role !== "string" || !ROLES.has(r.role)) throw new ConvoError("message role must be user, assistant, system or note");
    if (typeof r.content !== "string") throw new ConvoError("message content must be a string");
    messages.push(r as ConvoMsg);
  }
  const updated = Number(b.updated);
  if (!Number.isFinite(updated) || updated <= 0) throw new ConvoError("updated must be a unix time in ms");
  const title = (typeof b.title === "string" ? b.title : "").slice(0, 200) || "new chat";
  const model = (typeof b.model === "string" ? b.model : "").slice(0, 200) || "auto";
  return { id, title, model, messages, updated: Math.floor(updated) };
}

export function createConvoStore(sql: Sql, opts: { key: Buffer | null }) {
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of CONVO_SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));
  const needKey = () => {
    if (!opts.key) throw new ConvoError("conversation sync is not configured on this server (C0MPUTE_CHAT_KEY)", 503);
    return opts.key;
  };
  const aad = (userId: string, id: string) => `${userId}|${id}|messages`;

  function toConversation(r: Row, key: Buffer): Conversation | null {
    const userId = String(r.user_id);
    const id = String(r.id);
    const plain = typeof r.messages === "string" ? unseal(key, r.messages, aad(userId, id)) : null;
    if (plain === null) return null;
    let messages: ConvoMsg[] = [];
    try {
      messages = JSON.parse(plain);
    } catch {
      return null;
    }
    return { id, title: String(r.title), model: String(r.model), messages, updated: Number(r.updated) };
  }

  return {
    async list(userId: string, since?: number): Promise<ConvoSummary[]> {
      await init();
      const rows = since
        ? await sql.all(`SELECT id, title, model, updated, deleted FROM chat_conversations WHERE user_id = ? AND updated > ? ORDER BY updated DESC LIMIT ${MAX_CONVOS * 2}`, [userId, since])
        : await sql.all(`SELECT id, title, model, updated, deleted FROM chat_conversations WHERE user_id = ? AND deleted = 0 ORDER BY updated DESC LIMIT ${MAX_CONVOS}`, [userId]);
      return rows.map((r) => ({ id: String(r.id), title: String(r.title), model: String(r.model), updated: Number(r.updated), deleted: Number(r.deleted) === 1 }));
    },

    async get(userId: string, id: string): Promise<Conversation | null> {
      const key = needKey();
      await init();
      const [r] = await sql.all(`SELECT * FROM chat_conversations WHERE user_id = ? AND id = ? AND deleted = 0`, [userId, id]);
      return r ? toConversation(r, key) : null;
    },

    /** Save; throws ConvoError 409 (with the server copy) when the stored one is newer. */
    async put(userId: string, c: Conversation): Promise<{ id: string; updated: number }> {
      const key = needKey();
      await init();
      const plain = JSON.stringify(c.messages);
      const size = Buffer.byteLength(plain);
      if (size > MAX_BYTES) throw new ConvoError(`a conversation is at most ${MAX_BYTES} bytes; strip images and audio before syncing`, 413);
      const [existing] = await sql.all(`SELECT updated, deleted FROM chat_conversations WHERE user_id = ? AND id = ?`, [userId, c.id]);
      if (existing && Number(existing.updated) > c.updated) {
        const current = Number(existing.deleted) === 1 ? null : await this.get(userId, c.id);
        throw new ConvoError("a newer copy of this conversation is on the account", 409, current ?? undefined);
      }
      if (!existing) {
        const [n] = await sql.all(`SELECT COUNT(*) AS n FROM chat_conversations WHERE user_id = ? AND deleted = 0`, [userId]);
        if (Number(n?.n ?? 0) >= MAX_CONVOS) throw new ConvoError(`at most ${MAX_CONVOS} synced conversations; delete some first`, 413);
      }
      await sql.run(
        `INSERT INTO chat_conversations (user_id, id, title, model, messages, updated, deleted, size, created_at)
         VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?)
         ON CONFLICT (user_id, id) DO UPDATE SET title = excluded.title, model = excluded.model, messages = excluded.messages,
           updated = excluded.updated, deleted = 0, size = excluded.size
         WHERE excluded.updated >= chat_conversations.updated`,
        [userId, c.id, c.title, c.model, seal(key, plain, aad(userId, c.id)), c.updated, size, new Date().toISOString()],
      );
      return { id: c.id, updated: c.updated };
    },

    /** Tombstone: the messages go, the row stays so other devices learn of the delete. */
    async remove(userId: string, id: string, at = Date.now()): Promise<boolean> {
      await init();
      const [r] = await sql.all(`SELECT updated FROM chat_conversations WHERE user_id = ? AND id = ? AND deleted = 0`, [userId, id]);
      if (!r) return false;
      await sql.run(`UPDATE chat_conversations SET deleted = 1, messages = NULL, size = 0, updated = ? WHERE user_id = ? AND id = ?`, [Math.max(at, Number(r.updated) + 1), userId, id]);
      return true;
    },
  };
}

export type ConvoStore = ReturnType<typeof createConvoStore>;
