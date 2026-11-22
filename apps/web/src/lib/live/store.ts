// c0mpute Live: account-bound live streams (DIP-0019).
//
// A person creates a stream and gets a stream key. They push RTMP to the
// hosted ingest (rtmp://live.c0mpute.com/live/<key>); the ingest node asks us
// whether the key is good before it accepts any media, then broadcasts into
// the p2p network and tells us the network stream id it is on air under.
// Relays meter the bytes they serve and are paid per GB out of the streamer's
// balance; a stream with no balance runs on the free tier (capped, unpaid).
//
// The stream key is a secret: stored as a sha256, shown once at create and on
// rotate. The public id (lv_...) is what links carry.

import { createHash, randomBytes } from "node:crypto";
import type { Row, Sql } from "../chat/store";

export const LIVE_SCOPE = "live:publish";
export const LIVE_RTMP_URL = "rtmp://live.c0mpute.com/live";
/** What a relay earns, and the streamer pays, per GB (1e9 bytes) served. */
export const LIVE_USD_PER_GB = 0.01;
/** The free tier: a stream whose owner has no balance. Relays are not paid for it. */
export const FREE_MAX_HEIGHT = 720;
export const FREE_MAX_VIEWERS = 25;
export const MAX_STREAMS_PER_USER = 50;
/** The most one account is charged for live relay in an hour; past it the stream drops to the free tier. */
export const MAX_SPEND_USD_PER_HOUR = 25;

export const LIVE_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS live_streams (
     id          TEXT PRIMARY KEY,
     user_id     TEXT NOT NULL,
     coinpay_sub TEXT,
     name        TEXT NOT NULL,
     title       TEXT NOT NULL,
     visibility  TEXT NOT NULL DEFAULT 'public',
     key_hash    TEXT NOT NULL,
     network_id  TEXT,
     ingest_peer TEXT,
     live        INTEGER NOT NULL DEFAULT 0,
     paid        INTEGER NOT NULL DEFAULT 0,
     started_at  INTEGER,
     ended_at    INTEGER,
     created_at  INTEGER NOT NULL,
     deleted     INTEGER NOT NULL DEFAULT 0
   )`,
  `CREATE UNIQUE INDEX IF NOT EXISTS live_streams_key ON live_streams (key_hash)`,
  `CREATE INDEX IF NOT EXISTS live_streams_user ON live_streams (user_id, deleted)`,
  `CREATE INDEX IF NOT EXISTS live_streams_network ON live_streams (network_id)`,
  `CREATE TABLE IF NOT EXISTS live_usage (
     job_id     TEXT PRIMARY KEY,
     stream_id  TEXT NOT NULL,
     user_id    TEXT NOT NULL,
     node       TEXT NOT NULL,
     peer       TEXT,
     bytes      INTEGER NOT NULL,
     cost_usd   REAL NOT NULL,
     charged    INTEGER NOT NULL DEFAULT 0,
     at         INTEGER NOT NULL
   )`,
  `CREATE INDEX IF NOT EXISTS live_usage_stream ON live_usage (stream_id, at)`,
  `CREATE INDEX IF NOT EXISTS live_usage_user ON live_usage (user_id, at)`,
];

/** The gateway that serves playlists and segments (C0MPUTE_LIVE_GATEWAY_URL). */
export const liveGatewayUrl = () => (process.env.C0MPUTE_LIVE_GATEWAY_URL || "https://gateway.c0mpute.com").replace(/\/$/, "");

export const STREAM_ID =/^lv_[A-Za-z0-9_-]{12,24}$/;
const STREAM_KEY = /^c0l_[A-Za-z0-9_-]{24,64}$/;
const NAME = /^[a-z0-9][a-z0-9-]{0,62}$/;
const NETWORK_ID = /^[A-Za-z0-9_-]{8,128}$/;
const PEER = /^[A-Za-z0-9]{8,128}$/;

export type Visibility = "public" | "unlisted";
export type Tier = "paid" | "free";

export interface LiveStream {
  id: string;
  user_id: string;
  coinpay_sub: string | null;
  name: string;
  title: string;
  visibility: Visibility;
  network_id: string | null;
  live: boolean;
  paid: boolean;
  started_at: number | null;
  ended_at: number | null;
  created_at: number;
}

export class LiveError extends Error {
  constructor(
    message: string,
    public status = 400,
  ) {
    super(message);
  }
}

export const newStreamId = () => `lv_${randomBytes(12).toString("base64url")}`;
export const newStreamKey = () => `c0l_${randomBytes(24).toString("base64url")}`;
export const hashKey = (key: string) => createHash("sha256").update(key).digest("hex");

/** "My Board Meeting!" -> "my-board-meeting": what a stream is named on the network. */
export function slugName(raw: string): string {
  return raw
    .toLowerCase()
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 63)
    .replace(/-+$/g, "");
}

export function parseCreate(body: unknown): { name: string; title: string; visibility: Visibility } {
  if (!body || typeof body !== "object") throw new LiveError("body must be a JSON object");
  const b = body as Record<string, unknown>;
  if (typeof b.name !== "string" || !b.name.trim()) throw new LiveError("name is required");
  const name = slugName(b.name);
  if (!NAME.test(name)) throw new LiveError("name must have letters or digits (a-z, 0-9, -), at most 63");
  const title = (typeof b.title === "string" && b.title.trim() ? b.title : b.name).replace(/\s+/g, " ").trim().slice(0, 120);
  if (b.visibility !== undefined && b.visibility !== "public" && b.visibility !== "unlisted") throw new LiveError('visibility is "public" or "unlisted"');
  return { name, title, visibility: (b.visibility as Visibility | undefined) ?? "public" };
}

/** `live:<network stream id>:<peer>:<epoch>`: the job id a relay's receipt is credited under. */
export function parseRelayJobId(jobId: string): { network_id: string; peer: string; epoch: number } | null {
  const m = /^live:([A-Za-z0-9_-]{8,64}):([1-9A-HJ-NP-Za-km-z]{1,16}):(\d{1,12})$/.exec(jobId);
  return m ? { network_id: m[1], peer: m[2], epoch: Number(m[3]) } : null;
}

export const bytesForUsd = (usd: number) => Math.round((usd / LIVE_USD_PER_GB) * 1e9);

const toStream = (r: Row): LiveStream => ({
  id: String(r.id),
  user_id: String(r.user_id),
  coinpay_sub: r.coinpay_sub == null ? null : String(r.coinpay_sub),
  name: String(r.name),
  title: String(r.title),
  visibility: r.visibility === "unlisted" ? "unlisted" : "public",
  network_id: r.network_id == null ? null : String(r.network_id),
  live: Number(r.live) === 1,
  paid: Number(r.paid) === 1,
  started_at: r.started_at == null ? null : Number(r.started_at),
  ended_at: r.ended_at == null ? null : Number(r.ended_at),
  created_at: Number(r.created_at),
});

const COLS = "id, user_id, coinpay_sub, name, title, visibility, network_id, live, paid, started_at, ended_at, created_at";

export function createLiveStore(sql: Sql, opts: { now?: () => number; newId?: () => string; newKey?: () => string } = {}) {
  const now = opts.now ?? Date.now;
  const newId = opts.newId ?? newStreamId;
  const newKey = opts.newKey ?? newStreamKey;
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of LIVE_SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  return {
    /** A new stream and its key. The key is returned here and never again. */
    async create(user: { id: string; coinpay_sub: string | null }, body: unknown): Promise<{ stream: LiveStream; stream_key: string }> {
      const s = parseCreate(body);
      await init();
      const [dupe] = await sql.all(`SELECT id FROM live_streams WHERE user_id = ? AND name = ? AND deleted = 0`, [user.id, s.name]);
      if (dupe) throw new LiveError(`you already have a stream named "${s.name}" (${String(dupe.id)})`, 409);
      const [n] = await sql.all(`SELECT COUNT(*) AS n FROM live_streams WHERE user_id = ? AND deleted = 0`, [user.id]);
      if (Number(n?.n ?? 0) >= MAX_STREAMS_PER_USER) throw new LiveError(`at most ${MAX_STREAMS_PER_USER} streams; delete some first`, 429);
      const id = newId();
      const key = newKey();
      const t = now();
      await sql.run(`INSERT INTO live_streams (id, user_id, coinpay_sub, name, title, visibility, key_hash, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)`, [
        id,
        user.id,
        user.coinpay_sub,
        s.name,
        s.title,
        s.visibility,
        hashKey(key),
        t,
      ]);
      const [row] = await sql.all(`SELECT ${COLS} FROM live_streams WHERE id = ?`, [id]);
      return { stream: toStream(row), stream_key: key };
    },

    async list(userId: string): Promise<LiveStream[]> {
      await init();
      return (await sql.all(`SELECT ${COLS} FROM live_streams WHERE user_id = ? AND deleted = 0 ORDER BY created_at DESC`, [userId])).map(toStream);
    },

    /** The owner's stream, or null. */
    async get(userId: string, id: string): Promise<LiveStream | null> {
      if (!STREAM_ID.test(id)) return null;
      await init();
      const [r] = await sql.all(`SELECT ${COLS} FROM live_streams WHERE id = ? AND user_id = ? AND deleted = 0`, [id, userId]);
      return r ? toStream(r) : null;
    },

    /** Any stream by its public id, for the watch page and playlist redirect. */
    async publicGet(id: string): Promise<LiveStream | null> {
      if (!STREAM_ID.test(id)) return null;
      await init();
      const [r] = await sql.all(`SELECT ${COLS} FROM live_streams WHERE id = ? AND deleted = 0`, [id]);
      return r ? toStream(r) : null;
    },

    /** Public streams that are on air now, newest first. */
    async onAir(limit = 50): Promise<LiveStream[]> {
      await init();
      return (await sql.all(`SELECT ${COLS} FROM live_streams WHERE live = 1 AND deleted = 0 AND visibility = 'public' ORDER BY started_at DESC LIMIT ?`, [limit])).map(toStream);
    },

    /** Delete: the key stops working at once. */
    async remove(userId: string, id: string): Promise<boolean> {
      if (!(await this.get(userId, id))) return false;
      await sql.run(`UPDATE live_streams SET deleted = 1, live = 0, ended_at = ? WHERE id = ? AND user_id = ?`, [now(), id, userId]);
      return true;
    },

    /** A new key; the old one stops working at once. */
    async rotateKey(userId: string, id: string): Promise<string | null> {
      if (!(await this.get(userId, id))) return null;
      const key = newKey();
      await sql.run(`UPDATE live_streams SET key_hash = ? WHERE id = ? AND user_id = ?`, [hashKey(key), id, userId]);
      return key;
    },

    /** The stream a key publishes to, for the ingest node. */
    async byKey(key: unknown): Promise<LiveStream | null> {
      if (typeof key !== "string" || !STREAM_KEY.test(key)) return null;
      await init();
      const [r] = await sql.all(`SELECT ${COLS} FROM live_streams WHERE key_hash = ? AND deleted = 0`, [hashKey(key)]);
      return r ? toStream(r) : null;
    },

    /** The ingest node went on air with a stream, or took it off. */
    async ingestEvent(body: unknown): Promise<boolean> {
      const b = (body ?? {}) as Record<string, unknown>;
      if (typeof b.id !== "string" || !STREAM_ID.test(b.id)) throw new LiveError("id is required");
      if (b.event !== "start" && b.event !== "end") throw new LiveError('event is "start" or "end"');
      await init();
      const [r] = await sql.all(`SELECT id FROM live_streams WHERE id = ? AND deleted = 0`, [b.id]);
      if (!r) return false;
      if (b.event === "end") {
        await sql.run(`UPDATE live_streams SET live = 0, ended_at = ? WHERE id = ?`, [now(), b.id]);
        return true;
      }
      if (typeof b.stream_id !== "string" || !NETWORK_ID.test(b.stream_id)) throw new LiveError("stream_id is required on start");
      const peer = typeof b.peer === "string" && PEER.test(b.peer) ? b.peer : null;
      await sql.run(`UPDATE live_streams SET live = 1, network_id = ?, ingest_peer = ?, started_at = ?, ended_at = NULL WHERE id = ?`, [b.stream_id, peer, now(), b.id]);
      return true;
    },

    /** Whether the stream is on the paid tier, as the ingest node was last told. */
    async setPaid(id: string, paid: boolean): Promise<void> {
      await init();
      await sql.run(`UPDATE live_streams SET paid = ? WHERE id = ?`, [paid ? 1 : 0, id]);
    },

    /** The stream on air under a network stream id (the newest, if a name was reused). */
    async byNetworkId(networkId: string): Promise<LiveStream | null> {
      if (!NETWORK_ID.test(networkId)) return null;
      await init();
      // A receipt's job id carries at most the first 64 characters of the id.
      const [r] = await sql.all(`SELECT ${COLS} FROM live_streams WHERE network_id = ? OR substr(network_id, 1, 64) = ? ORDER BY started_at DESC LIMIT 1`, [networkId, networkId]);
      return r ? toStream(r) : null;
    },

    /** File one relay receipt. False when it was already filed: a receipt settles once. */
    async fileUsage(u: { job_id: string; stream_id: string; user_id: string; node: string; peer: string | null; bytes: number; cost_usd: number; at: number }): Promise<boolean> {
      await init();
      const [seen] = await sql.all(`SELECT job_id FROM live_usage WHERE job_id = ?`, [u.job_id]);
      if (seen) return false;
      await sql.run(`INSERT INTO live_usage (job_id, stream_id, user_id, node, peer, bytes, cost_usd, charged, at) VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?)`, [
        u.job_id,
        u.stream_id,
        u.user_id,
        u.node,
        u.peer,
        u.bytes,
        u.cost_usd,
        u.at,
      ]);
      return true;
    },

    /** Close a receipt: 1 = the streamer was charged, 2 = nothing to charge it to (it stays unpaid). */
    async settle(jobId: string, state: 1 | 2): Promise<void> {
      await sql.run(`UPDATE live_usage SET charged = ? WHERE job_id = ?`, [state, jobId]);
    },

    /** Receipts filed and not yet charged, oldest first (a charge that failed is retried). */
    async uncharged(limit = 200): Promise<{ job_id: string; stream_id: string; user_id: string; node: string; peer: string | null; bytes: number; cost_usd: number; at: number }[]> {
      await init();
      return (await sql.all(`SELECT job_id, stream_id, user_id, node, peer, bytes, cost_usd, at FROM live_usage WHERE charged = 0 AND cost_usd > 0 ORDER BY at ASC LIMIT ?`, [limit])).map((r) => ({
        job_id: String(r.job_id),
        stream_id: String(r.stream_id),
        user_id: String(r.user_id),
        node: String(r.node),
        peer: r.peer == null ? null : String(r.peer),
        bytes: Number(r.bytes),
        cost_usd: Number(r.cost_usd),
        at: Number(r.at),
      }));
    },

    /** What an account was charged since `since` (the hourly spend cap reads this). */
    async spentSince(userId: string, since: number): Promise<number> {
      await init();
      const [r] = await sql.all(`SELECT COALESCE(SUM(cost_usd), 0) AS usd FROM live_usage WHERE user_id = ? AND charged = 1 AND at >= ?`, [userId, since]);
      return Number(r?.usd ?? 0);
    },

    async usage(streamId: string): Promise<{ bytes: number; gb: number; cost_usd: number; charged_usd: number; epochs: number }> {
      await init();
      const [r] = await sql.all(
        `SELECT COALESCE(SUM(bytes), 0) AS bytes, COALESCE(SUM(cost_usd), 0) AS usd, COALESCE(SUM(CASE WHEN charged = 1 THEN cost_usd ELSE 0 END), 0) AS charged, COUNT(*) AS n FROM live_usage WHERE stream_id = ?`,
        [streamId],
      );
      const bytes = Number(r?.bytes ?? 0);
      return { bytes, gb: bytes / 1e9, cost_usd: Number(r?.usd ?? 0), charged_usd: Number(r?.charged ?? 0), epochs: Number(r?.n ?? 0) };
    },
  };
}

export type LiveStore = ReturnType<typeof createLiveStore>;

/** What the API says about a stream. Never the key. */
export function streamJson(s: LiveStream, site: string) {
  return {
    id: s.id,
    name: s.name,
    title: s.title,
    visibility: s.visibility,
    live: s.live,
    tier: (s.paid ? "paid" : "free") as Tier,
    watch_url: `${site}/live/${s.id}`,
    playback_url: `${site}/live/${s.id}/index.m3u8`,
    rtmp_url: LIVE_RTMP_URL,
    started_at: s.started_at,
    ended_at: s.ended_at,
    created_at: s.created_at,
  };
}
