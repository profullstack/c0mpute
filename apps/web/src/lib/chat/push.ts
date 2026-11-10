// `/push hosting <name>` in c0mpute chat: publish the files a chat produced
// (images, songs, album zips, promo videos, the transcript) as a c0mpute
// hosting site, paid on the d1sks marketplace (`hosting publish --disks auto`).
//
// c0mpute.com never publishes anything itself. A person links one of their own
// nodes to their chat account (`/connect hosting` -> `c0mpute chat connect
// hosting --name <node>` on that machine, signed in with the same CoinPay
// account). That node polls for push jobs, downloads the staged files, signs
// the site with its own key, pays its hosts from its own CoinPay wallet
// ([payments] rail = coinpay) and reports back. So every site is owned by the
// person cryptographically, and every push is a row under their account that
// only they can list, update or delete.

import { randomBytes } from "node:crypto";
import type { Row, Sql } from "./store";
import { CLAIM_TIMEOUT_MS, MAX_NODES, MAX_PUSHES, NODE_NAME, ONLINE_MS, SITE_NAME, type PushFile, type Target } from "./push-shared";

export * from "./push-shared";

export type PushStatus = "queued" | "publishing" | "live" | "failed" | "deleting";

export type HostingNode = {
  id: string;
  user_id: string;
  name: string;
  peer_id: string | null;
  payments: string | null;
  version: string | null;
  last_seen_at: string | null;
  created_at: string;
};

export type Push = {
  id: string;
  user_id: string;
  node_id: string;
  target: Target;
  name: string;
  status: PushStatus;
  rev: number;
  files: PushFile[];
  bytes: number;
  disks: string;
  site_id: string | null;
  version: number | null;
  url: string | null;
  copies: number | null;
  error: string | null;
  /** The node's latest progress line while publishing or deleting. */
  progress: string | null;
  claimed_at: string | null;
  created_at: string;
  updated_at: string;
};

export const PUSH_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS chat_nodes (
     id           TEXT PRIMARY KEY,
     user_id      TEXT NOT NULL,
     name         TEXT NOT NULL,
     peer_id      TEXT,
     payments     TEXT,
     version      TEXT,
     last_seen_at TEXT,
     created_at   TEXT NOT NULL,
     UNIQUE (user_id, name)
   )`,
  `CREATE TABLE IF NOT EXISTS chat_pushes (
     id         TEXT PRIMARY KEY,
     user_id    TEXT NOT NULL,
     node_id    TEXT NOT NULL,
     target     TEXT NOT NULL,
     name       TEXT NOT NULL,
     status     TEXT NOT NULL,
     rev        INTEGER NOT NULL DEFAULT 1,
     files      TEXT NOT NULL DEFAULT '[]',
     bytes      INTEGER NOT NULL DEFAULT 0,
     disks      TEXT NOT NULL DEFAULT 'auto',
     site_id    TEXT,
     version    INTEGER,
     url        TEXT,
     copies     INTEGER,
     error      TEXT,
     progress   TEXT,
     claimed_at TEXT,
     created_at TEXT NOT NULL,
     updated_at TEXT NOT NULL,
     UNIQUE (user_id, target, name)
   )`,
  `CREATE INDEX IF NOT EXISTS idx_chat_pushes_node ON chat_pushes(node_id, status)`,
];

// ── store ───────────────────────────────────────────────────────────────────

const now = () => new Date().toISOString();
const id = (p: string) => p + randomBytes(12).toString("hex");

function toPush(r: Row): Push {
  let files: PushFile[] = [];
  try {
    files = JSON.parse(String(r.files ?? "[]"));
  } catch {}
  return {
    id: String(r.id),
    user_id: String(r.user_id),
    node_id: String(r.node_id),
    target: r.target as Target,
    name: String(r.name),
    status: r.status as PushStatus,
    rev: Number(r.rev ?? 1),
    files,
    bytes: Number(r.bytes ?? 0),
    disks: String(r.disks ?? "auto"),
    site_id: (r.site_id as string) ?? null,
    version: r.version == null ? null : Number(r.version),
    url: (r.url as string) ?? null,
    copies: r.copies == null ? null : Number(r.copies),
    error: (r.error as string) ?? null,
    progress: (r.progress as string) ?? null,
    claimed_at: (r.claimed_at as string) ?? null,
    created_at: String(r.created_at),
    updated_at: String(r.updated_at),
  };
}

/** What an owner (or their node) sees of a push. */
export function publicPush(p: Push, node?: HostingNode | null) {
  return {
    id: p.id,
    target: p.target,
    name: p.name,
    status: p.status,
    rev: p.rev,
    node: node?.name ?? null,
    files: p.files.map((f) => ({ path: f.path, type: f.type })),
    bytes: p.bytes,
    disks: p.disks,
    site_id: p.site_id,
    version: p.version,
    url: p.url,
    copies: p.copies,
    error: p.error,
    progress: p.status === "queued" || p.status === "publishing" || p.status === "deleting" ? p.progress : null,
    created_at: p.created_at,
    updated_at: p.updated_at,
  };
}

/**
 * A job as the node gets it: inline files carry their text, media files a
 * download path on c0mpute.com (`/api/v1/hosting/pushes/<id>/files/<n>`,
 * fetched with the same account key).
 */
export function nodeJob(p: Push) {
  return {
    id: p.id,
    rev: p.rev,
    action: p.status === "deleting" ? "rm" : "publish",
    name: p.name,
    disks: p.disks,
    site_id: p.site_id,
    files:
      p.status === "deleting"
        ? []
        : p.files.map((f, n) =>
            f.text !== undefined
              ? { path: f.path, type: f.type, text: f.text }
              : { path: f.path, type: f.type, url: `/api/v1/hosting/pushes/${p.id}/files/${n}` },
          ),
  };
}

export function publicNode(n: HostingNode, at = Date.now()) {
  return {
    id: n.id,
    name: n.name,
    peer_id: n.peer_id,
    payments: n.payments,
    version: n.version,
    online: !!n.last_seen_at && at - Date.parse(n.last_seen_at) < ONLINE_MS,
    last_seen_at: n.last_seen_at,
    created_at: n.created_at,
  };
}

export class PushError extends Error {
  constructor(
    message: string,
    public status = 400,
  ) {
    super(message);
  }
}

export function createPushStore(sql: Sql) {
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of PUSH_SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  async function one(q: string, p: unknown[]): Promise<Row | null> {
    await init();
    return (await sql.all(q, p))[0] ?? null;
  }

  const store = {
    init,

    // ── nodes ────────────────────────────────────────────────────────────────
    /** A node signs in with the account's key and (re)registers by name. */
    async registerNode(
      userId: string,
      info: { name: string; peer_id?: unknown; payments?: unknown; version?: unknown },
    ): Promise<HostingNode> {
      const name = String(info.name ?? "").toLowerCase();
      if (!NODE_NAME.test(name)) throw new PushError("node name: 1-63 of a-z, 0-9 and '-'");
      const str = (v: unknown, n: number) => (typeof v === "string" && v ? v.slice(0, n) : null);
      const peer = str(info.peer_id, 128);
      const payments = str(info.payments, 32);
      const version = str(info.version, 32);
      const existing = await one(`SELECT * FROM chat_nodes WHERE user_id = ? AND name = ?`, [userId, name]);
      if (existing) {
        await sql.run(`UPDATE chat_nodes SET peer_id = ?, payments = ?, version = ?, last_seen_at = ? WHERE id = ?`, [
          peer,
          payments,
          version,
          now(),
          existing.id,
        ]);
        return (await store.node(userId, String(existing.id)))!;
      }
      const count = await one(`SELECT COUNT(*) AS n FROM chat_nodes WHERE user_id = ?`, [userId]);
      if (Number(count?.n ?? 0) >= MAX_NODES) throw new PushError(`at most ${MAX_NODES} nodes per account; remove one first`, 409);
      const nid = id("n_");
      await sql.run(
        `INSERT INTO chat_nodes (id, user_id, name, peer_id, payments, version, last_seen_at, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
        [nid, userId, name, peer, payments, version, now(), now()],
      );
      return (await store.node(userId, nid))!;
    },

    async node(userId: string, idOrName: string): Promise<HostingNode | null> {
      const r = await one(`SELECT * FROM chat_nodes WHERE user_id = ? AND (id = ? OR name = ?)`, [userId, idOrName, idOrName]);
      return (r as unknown as HostingNode) ?? null;
    },

    async nodes(userId: string): Promise<HostingNode[]> {
      await init();
      return (await sql.all(`SELECT * FROM chat_nodes WHERE user_id = ? ORDER BY created_at`, [userId])) as unknown as HostingNode[];
    },

    /** Unlink a node. Its sites stay on the network; their pushes are orphaned until another node takes them over. */
    async removeNode(userId: string, idOrName: string) {
      const n = await store.node(userId, idOrName);
      if (!n) return false;
      const busy = await one(`SELECT COUNT(*) AS n FROM chat_pushes WHERE node_id = ? AND status IN ('queued','publishing','deleting')`, [n.id]);
      if (Number(busy?.n ?? 0) > 0) throw new PushError(`${n.name} still has pushes in flight`, 409);
      await sql.run(`DELETE FROM chat_nodes WHERE id = ?`, [n.id]);
      return true;
    },

    // ── pushes (owner CRUD) ──────────────────────────────────────────────────
    async push(userId: string, target: Target, name: string): Promise<Push | null> {
      const r = await one(`SELECT * FROM chat_pushes WHERE user_id = ? AND target = ? AND name = ?`, [userId, target, name]);
      return r ? toPush(r) : null;
    },

    async pushById(userId: string, pid: string): Promise<Push | null> {
      const r = await one(`SELECT * FROM chat_pushes WHERE user_id = ? AND id = ?`, [userId, pid]);
      return r ? toPush(r) : null;
    },

    async pushes(userId: string, target?: Target): Promise<Push[]> {
      await init();
      const rows = target
        ? await sql.all(`SELECT * FROM chat_pushes WHERE user_id = ? AND target = ? ORDER BY updated_at DESC`, [userId, target])
        : await sql.all(`SELECT * FROM chat_pushes WHERE user_id = ? ORDER BY updated_at DESC`, [userId]);
      return rows.map(toPush);
    },

    /**
     * Create a push, or ship a new version of an existing one (same name). The
     * site keeps its node: a site is signed by its node's key, so only that
     * node can update it.
     */
    async upsertPush(
      userId: string,
      p: { target: Target; name: string; node: HostingNode; files: PushFile[]; disks?: string },
    ): Promise<Push> {
      if (!SITE_NAME.test(p.name)) throw new PushError("site name: 1-63 of a-z, 0-9 and '-'");
      if (!p.files.length) throw new PushError("nothing to push: this chat has no files yet");
      const bytes = 0; // the node reports the real size once it has fetched the files
      const files = JSON.stringify(p.files);
      const disks = p.disks ?? "auto";
      const existing = await store.push(userId, p.target, p.name);
      if (existing) {
        if (existing.status === "publishing" || existing.status === "deleting")
          throw new PushError(`${p.name} is ${existing.status}; wait for it to finish`, 409);
        if (existing.site_id && existing.node_id !== p.node.id) {
          const owner = await store.node(userId, existing.node_id);
          throw new PushError(
            `${p.name} is signed by node ${owner?.name ?? existing.node_id}; only that node can update it (or /push hosting rm ${p.name} first)`,
            409,
          );
        }
        await sql.run(
          `UPDATE chat_pushes SET node_id = ?, status = 'queued', rev = rev + 1, files = ?, bytes = ?, disks = ?,
                  error = NULL, progress = NULL, claimed_at = NULL, updated_at = ? WHERE id = ?`,
          [p.node.id, files, bytes, disks, now(), existing.id],
        );
        return (await store.pushById(userId, existing.id))!;
      }
      const count = await one(`SELECT COUNT(*) AS n FROM chat_pushes WHERE user_id = ?`, [userId]);
      if (Number(count?.n ?? 0) >= MAX_PUSHES) throw new PushError(`at most ${MAX_PUSHES} pushes per account; remove one first`, 409);
      const pid = id("p_");
      await sql.run(
        `INSERT INTO chat_pushes (id, user_id, node_id, target, name, status, rev, files, bytes, disks, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 'queued', 1, ?, ?, ?, ?, ?)`,
        [pid, userId, p.node.id, p.target, p.name, files, bytes, disks, now(), now()],
      );
      return (await store.pushById(userId, pid))!;
    },

    /**
     * Delete: a push that never reached the network goes at once; a live site
     * becomes a "deleting" job for its node (`hosting rm`), and the row goes
     * when the node confirms.
     */
    async deletePush(userId: string, target: Target, name: string): Promise<"gone" | "deleting" | null> {
      const p = await store.push(userId, target, name);
      if (!p) return null;
      if (p.status === "publishing") throw new PushError(`${name} is publishing; wait for it to finish`, 409);
      if (!p.site_id) {
        await sql.run(`DELETE FROM chat_pushes WHERE id = ?`, [p.id]);
        return "gone";
      }
      await sql.run(`UPDATE chat_pushes SET status = 'deleting', claimed_at = NULL, error = NULL, progress = NULL, updated_at = ? WHERE id = ?`, [
        now(),
        p.id,
      ]);
      return "deleting";
    },

    // ── node side ────────────────────────────────────────────────────────────
    /**
     * The node's poll: marks it seen and hands out its work. Queued pushes are
     * claimed (-> publishing); a claim older than CLAIM_TIMEOUT_MS is handed
     * out again; deletes stay "deleting" until reported.
     */
    async claimJobs(userId: string, nodeId: string): Promise<Push[]> {
      const n = await store.node(userId, nodeId);
      if (!n) throw new PushError("unknown node; run c0mpute chat connect hosting again", 404);
      const t = now();
      await sql.run(`UPDATE chat_nodes SET last_seen_at = ? WHERE id = ?`, [t, n.id]);
      const stale = new Date(Date.now() - CLAIM_TIMEOUT_MS).toISOString();
      const rows = await sql.all(
        `SELECT * FROM chat_pushes WHERE user_id = ? AND node_id = ?
           AND (status = 'queued' OR (status IN ('publishing','deleting') AND (claimed_at IS NULL OR claimed_at < ?)))
         ORDER BY updated_at LIMIT 5`,
        [userId, n.id, stale],
      );
      const out: Push[] = [];
      for (const r of rows) {
        const p = toPush(r);
        const status = p.status === "deleting" ? "deleting" : "publishing";
        const progress = status === "deleting" ? "node picked up the delete" : "node picked it up";
        await sql.run(`UPDATE chat_pushes SET status = ?, claimed_at = ?, progress = ?, updated_at = ? WHERE id = ?`, [status, t, progress, t, p.id]);
        out.push({ ...p, status, claimed_at: t, progress });
      }
      return out;
    },

    /** The node's report for one job. `rev` must match, so a stale publish never overwrites a newer push. */
    async report(
      userId: string,
      nodeId: string,
      pid: string,
      r: { rev?: unknown; progress?: unknown; bytes?: unknown; note?: unknown; ok?: unknown; site_id?: unknown; version?: unknown; url?: unknown; copies?: unknown; error?: unknown; deleted?: unknown },
    ): Promise<Push | "deleted"> {
      const p = await store.pushById(userId, pid);
      if (!p || p.node_id !== nodeId) throw new PushError("no such push on this node", 404);
      if (Number(r.rev) !== p.rev) throw new PushError(`stale report: push is at rev ${p.rev}`, 409);
      const t = now();
      // A progress line only: keep the job alive (claimed_at) and show it.
      if (r.ok === undefined && typeof r.progress === "string") {
        if (p.status !== "publishing" && p.status !== "deleting") throw new PushError(`push is ${p.status}`, 409);
        await sql.run(`UPDATE chat_pushes SET progress = ?, claimed_at = ?, updated_at = ? WHERE id = ?`, [
          r.progress.replace(/[\u0000-\u001f\u007f]/g, " ").trim().slice(0, 300),
          t,
          t,
          p.id,
        ]);
        await sql.run(`UPDATE chat_nodes SET last_seen_at = ? WHERE id = ?`, [t, nodeId]);
        return (await store.pushById(userId, p.id))!;
      }
      if (p.status === "deleting") {
        if (r.ok === true || r.deleted === true) {
          await sql.run(`DELETE FROM chat_pushes WHERE id = ?`, [p.id]);
          return "deleted";
        }
        // Back to live with the error shown; `/push hosting rm` again retries.
        await sql.run(`UPDATE chat_pushes SET status = 'live', error = ?, progress = NULL, claimed_at = NULL, updated_at = ? WHERE id = ?`, [
          `delete failed: ${String(r.error ?? "unknown error")}`.slice(0, 1000),
          t,
          p.id,
        ]);
        return (await store.pushById(userId, p.id))!;
      }
      if (p.status !== "publishing") throw new PushError(`push is ${p.status}, not publishing`, 409);
      if (r.ok === true) {
        const siteId = typeof r.site_id === "string" && /^[0-9a-f]{8,64}$/.test(r.site_id) ? r.site_id : null;
        if (!siteId) throw new PushError("site_id missing");
        const url = typeof r.url === "string" && /^https?:\/\//.test(r.url) ? r.url.slice(0, 500) : null;
        const bytes = Number.isFinite(Number(r.bytes)) ? Math.max(0, Math.floor(Number(r.bytes))) : 0;
        await sql.run(
          `UPDATE chat_pushes SET status = 'live', site_id = ?, version = ?, bytes = ?, url = ?, copies = ?, error = ?, progress = NULL, claimed_at = NULL, updated_at = ? WHERE id = ?`,
          [
            siteId,
            Number.isFinite(Number(r.version)) ? Number(r.version) : null,
            bytes,
            url,
            Number(r.copies) || 0,
            typeof r.note === "string" && r.note ? r.note.slice(0, 1000) : null, // e.g. files left out
            t,
            p.id,
          ],
        );
      } else {
        await sql.run(`UPDATE chat_pushes SET status = 'failed', error = ?, progress = NULL, claimed_at = NULL, updated_at = ? WHERE id = ?`, [
          String(r.error ?? "publish failed").slice(0, 1000),
          t,
          p.id,
        ]);
      }
      return (await store.pushById(userId, p.id))!;
    },
  };
  return store;
}

export type PushStore = ReturnType<typeof createPushStore>;
