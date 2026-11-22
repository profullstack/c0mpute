// Shared chats (2026-11-22, Anthony: "sharable links for chats ... like my mom
// sent me a shared ChatGPT chat ... she's 81"). Sharing a chat stores a
// read-only snapshot under a random id; anyone with the link reads it at
// /share/<id> without signing in, and can continue it in their own chat.
//
// A snapshot is what the person chose to publish, so it is stored plain (not
// sealed like synced conversations). Sharing the same chat again updates the
// same link, so a mom who shares twice still sends one link. Unsharing marks
// the row revoked and the page answers 404 from then on.

import { randomBytes } from "node:crypto";
import type { Row, Sql } from "./store";

export const SHARE_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS chat_shares (
     id         TEXT PRIMARY KEY,
     user_id    TEXT NOT NULL,
     convo_id   TEXT NOT NULL,
     title      TEXT NOT NULL,
     messages   TEXT NOT NULL,
     created    INTEGER NOT NULL,
     updated    INTEGER NOT NULL,
     revoked    INTEGER NOT NULL DEFAULT 0,
     views      INTEGER NOT NULL DEFAULT 0
   )`,
  `CREATE INDEX IF NOT EXISTS chat_shares_user ON chat_shares (user_id, convo_id)`,
];

export const SHARE_ID = /^[A-Za-z0-9_-]{10,32}$/;
const CONVO_ID = /^[A-Za-z0-9_-]{4,64}$/;
export const MAX_SHARES_PER_USER = 500;
export const MAX_SHARE_MESSAGES = 200;
export const MAX_SHARE_BYTES = 512 * 1024;
const MAX_CONTENT = 40_000;

export type ShareSource = { n: number; title: string; url: string; host: string; image: string | null; snippet: string };
export type ShareMsg = { role: "user" | "assistant"; content: string; sources?: ShareSource[] };
export type Share = { id: string; title: string; messages: ShareMsg[]; created: number; updated: number };
export type ShareSummary = { id: string; convo_id: string; title: string; updated: number; views: number };

export class ShareError extends Error {
  constructor(
    message: string,
    public status = 400,
  ) {
    super(message);
  }
}

const httpUrl = (u: unknown): u is string => typeof u === "string" && /^https?:\/\/[^\s<>"]{1,2000}$/.test(u);

function cleanSources(raw: unknown): ShareSource[] | undefined {
  if (!Array.isArray(raw)) return undefined;
  const out: ShareSource[] = [];
  for (const r of raw.slice(0, 10)) {
    const s = r as Record<string, unknown>;
    if (!httpUrl(s?.url) || typeof s.n !== "number") continue;
    out.push({
      n: Math.trunc(s.n),
      url: s.url,
      title: String(s.title ?? s.url).slice(0, 200),
      host: String(s.host ?? "").slice(0, 100),
      image: typeof s.image === "string" && /^https:\/\/[^\s<>"]{1,2000}$/.test(s.image) ? s.image : null,
      snippet: String(s.snippet ?? "").slice(0, 300),
    });
  }
  return out.length ? out : undefined;
}

/**
 * What a share may hold: the text of user and assistant turns (and a grounded
 * answer's sources). Tool runs, media blobs, run ids, status lines and empty
 * placeholder turns are left out; "// ..." progress notes too.
 */
export function parseShareBody(body: unknown): { convoId: string; title: string; messages: ShareMsg[] } {
  if (!body || typeof body !== "object") throw new ShareError("body must be a JSON object");
  const b = body as Record<string, unknown>;
  const convoId = typeof b.convo_id === "string" && CONVO_ID.test(b.convo_id) ? b.convo_id : null;
  if (!convoId) throw new ShareError("convo_id is required");
  if (!Array.isArray(b.messages)) throw new ShareError("messages[] is required");
  const messages: ShareMsg[] = [];
  for (const m of b.messages as Record<string, unknown>[]) {
    if (!m || (m.role !== "user" && m.role !== "assistant") || typeof m.content !== "string") continue;
    const content = m.content.trim();
    if (!content || (m.role === "assistant" && /^\/\/ /.test(content) && !content.includes("\n"))) continue;
    const msg: ShareMsg = { role: m.role, content: content.slice(0, MAX_CONTENT) };
    const sources = m.role === "assistant" ? cleanSources(m.sources) : undefined;
    if (sources) msg.sources = sources;
    messages.push(msg);
  }
  if (!messages.some((m) => m.role === "assistant")) throw new ShareError("there is no answer in this chat to share yet");
  if (messages.length > MAX_SHARE_MESSAGES) throw new ShareError(`at most ${MAX_SHARE_MESSAGES} messages in a shared chat`);
  if (JSON.stringify(messages).length > MAX_SHARE_BYTES) throw new ShareError("this chat is too long to share (512 KB)");
  const first = messages.find((m) => m.role === "user")?.content ?? "Shared chat";
  const rawTitle = typeof b.title === "string" && b.title.trim() && b.title.trim() !== "new chat" ? b.title : first;
  return { convoId, title: rawTitle.replace(/\s+/g, " ").trim().slice(0, 120), messages };
}

export const newShareId = () => randomBytes(12).toString("base64url");

export function createShareStore(sql: Sql, opts: { now?: () => number; newId?: () => string } = {}) {
  const now = opts.now ?? Date.now;
  const newId = opts.newId ?? newShareId;
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of SHARE_SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  const toShare = (r: Row): Share | null => {
    try {
      return { id: String(r.id), title: String(r.title), messages: JSON.parse(String(r.messages)) as ShareMsg[], created: Number(r.created), updated: Number(r.updated) };
    } catch {
      return null;
    }
  };

  return {
    /** Share (or re-share) a chat: the same chat keeps its link. */
    async put(userId: string, body: unknown): Promise<{ id: string; created: boolean }> {
      const s = parseShareBody(body);
      await init();
      const t = now();
      const [existing] = await sql.all(`SELECT id FROM chat_shares WHERE user_id = ? AND convo_id = ? AND revoked = 0 ORDER BY created DESC LIMIT 1`, [userId, s.convoId]);
      if (existing) {
        const id = String(existing.id);
        await sql.run(`UPDATE chat_shares SET title = ?, messages = ?, updated = ? WHERE id = ? AND user_id = ?`, [s.title, JSON.stringify(s.messages), t, id, userId]);
        return { id, created: false };
      }
      const [n] = await sql.all(`SELECT COUNT(*) AS n FROM chat_shares WHERE user_id = ? AND revoked = 0`, [userId]);
      if (Number(n?.n ?? 0) >= MAX_SHARES_PER_USER) throw new ShareError(`at most ${MAX_SHARES_PER_USER} shared chats; unshare some first`, 429);
      const id = newId();
      await sql.run(`INSERT INTO chat_shares (id, user_id, convo_id, title, messages, created, updated, revoked, views) VALUES (?, ?, ?, ?, ?, ?, ?, 0, 0)`, [
        id,
        userId,
        s.convoId,
        s.title,
        JSON.stringify(s.messages),
        t,
        t,
      ]);
      return { id, created: true };
    },

    /** A live share, for anyone with the link. */
    async get(id: string): Promise<Share | null> {
      if (!SHARE_ID.test(id)) return null;
      await init();
      const [r] = await sql.all(`SELECT * FROM chat_shares WHERE id = ? AND revoked = 0`, [id]);
      return r ? toShare(r) : null;
    },

    async view(id: string): Promise<void> {
      if (!SHARE_ID.test(id)) return;
      await init();
      await sql.run(`UPDATE chat_shares SET views = views + 1 WHERE id = ? AND revoked = 0`, [id]);
    },

    /** The live share of one of this account's chats, if any (so the UI can say "shared"). */
    async forConvo(userId: string, convoId: string): Promise<string | null> {
      await init();
      const [r] = await sql.all(`SELECT id FROM chat_shares WHERE user_id = ? AND convo_id = ? AND revoked = 0 ORDER BY created DESC LIMIT 1`, [userId, convoId]);
      return r ? String(r.id) : null;
    },

    async list(userId: string): Promise<ShareSummary[]> {
      await init();
      const rows = await sql.all(`SELECT id, convo_id, title, updated, views FROM chat_shares WHERE user_id = ? AND revoked = 0 ORDER BY updated DESC LIMIT ${MAX_SHARES_PER_USER}`, [userId]);
      return rows.map((r) => ({ id: String(r.id), convo_id: String(r.convo_id), title: String(r.title), updated: Number(r.updated), views: Number(r.views) }));
    },

    /** Unshare: the link stops working. Only the owner can. */
    async revoke(userId: string, id: string): Promise<boolean> {
      if (!SHARE_ID.test(id)) return false;
      await init();
      const [r] = await sql.all(`SELECT id FROM chat_shares WHERE id = ? AND user_id = ? AND revoked = 0`, [id, userId]);
      if (!r) return false;
      await sql.run(`UPDATE chat_shares SET revoked = 1, updated = ? WHERE id = ? AND user_id = ?`, [now(), id, userId]);
      return true;
    },
  };
}

export type ShareStore = ReturnType<typeof createShareStore>;
