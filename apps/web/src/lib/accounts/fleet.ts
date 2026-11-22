// Fleet management: an operator's nodes in one view, and actions on one node
// or many (by selection or by tag) from any surface (web, PWA, desktop, TUI,
// CLI, API, MCP).
//
// Two kinds of action:
//   - account-side (rename, tag, untag, unlink, cost): changed here at once;
//   - node-side (pause, resume, update, restart, and the node settings
//     models, house-models, uncensored, hosting, storage): queued as a command the node
//     fetches on its own poll (authenticated by its identity key), carrying an
//     ed25519 signature by this site's fleet key that the node checks against
//     the key it pinned when it linked, along with its own peer id and the
//     account it is linked to. Only nodes whose identity key signed a link to
//     this account (verified) can be sent commands.
// Every action is written to the account's audit log.

import { createHash, createPrivateKey, createPublicKey, generateKeyPairSync, randomBytes, sign as edSign, type KeyObject } from "node:crypto";
import type { Row, Sql } from "../chat/store";
import { nodeKey, type OperatorNode } from "../earnings/store";
import type { NodeRow } from "../nodes";
import type { AccountStore, NodeMeta } from "./store";
import { cleanTags } from "./store";

export const NODE_ACTIONS = ["pause", "resume", "update", "restart", "models", "house-models", "uncensored", "hosting", "storage"] as const;
/** Node settings: applied by the node to `<config dir>/fleet-settings.json` (node/crates/c0mpute-core/src/fleet_settings.rs). */
export const SETTING_ACTIONS = ["models", "house-models", "uncensored", "hosting", "storage"] as const;
/** Most models one allowlist may name (fleet_settings.rs MAX_MODELS). */
export const MAX_MODELS = 64;
export const ACCOUNT_ACTIONS = ["rename", "tag", "untag", "set-tags", "unlink", "cost"] as const;
export type NodeAction = (typeof NODE_ACTIONS)[number];
export type AccountAction = (typeof ACCOUNT_ACTIONS)[number];
export type FleetAction = NodeAction | AccountAction;
/** A queued command stays fetchable this long; an offline node never runs a stale one. */
export const COMMAND_TTL_MS = 24 * 3_600_000;
/** A node counts as online when an aggregator heard it this recently. */
export const ONLINE_S = 180;
export const MAX_BULK = 500;

export const isNodeAction = (a: string): a is NodeAction => (NODE_ACTIONS as readonly string[]).includes(a);
export const isFleetAction = (a: string): a is FleetAction => isNodeAction(a) || (ACCOUNT_ACTIONS as readonly string[]).includes(a);

/** Model ids, the same rule the node applies (fleet_settings.rs `valid_model_id`). */
export const validModelId = (m: string) => m.length > 0 && m.length <= 128 && /^[A-Za-z0-9\-_.:/@]+$/.test(m);

/**
 * The args a node-side action carries (signed with the command), or why they
 * are refused. pause/resume/update/restart carry none; the settings:
 *   models {models: string[]} (empty = every model the node has),
 *   house-models | uncensored | hosting | storage {on: true | false | null}
 *   (null hands the setting back to the node's own config).
 */
export function nodeArgs(action: NodeAction, args: Record<string, unknown>): Record<string, unknown> | string {
  if (action === "models") {
    const raw = args.models;
    const list = Array.isArray(raw) ? raw : typeof raw === "string" ? raw.split(",") : raw == null ? [] : null;
    if (!list) return "models: a list of model ids (empty = every model the node has)";
    const out: string[] = [];
    for (const m of list) {
      const id = typeof m === "string" ? m.trim() : "";
      if (!id) continue;
      if (!validModelId(id)) return `not a model id: ${JSON.stringify(m)}`;
      if (!out.includes(id)) out.push(id);
    }
    if (out.length > MAX_MODELS) return `at most ${MAX_MODELS} models`;
    return { models: out };
  }
  if ((SETTING_ACTIONS as readonly string[]).includes(action)) {
    const on = args.on;
    if (on === true || on === false || on === null) return { on };
    if (on === "on" || on === "off") return { on: on === "on" };
    return `${action}: args {on: true | false} (or null for the node's own config)`;
  }
  return {};
}

/** What a node reports about itself on each poll (fleet_agent.rs `node_state`). */
export type NodeState = {
  v: number;
  version: string | null;
  paused: boolean | null;
  settings: { models: string[] | null; house_models: boolean | null; uncensored: boolean | null; hosting: boolean | null; storage: boolean | null };
  effective: { served_models: string[]; house_models: boolean | null; uncensored: boolean | null; hosting: boolean | null; storage: boolean | null };
  disk: { free_bytes: number; total_bytes: number } | null;
  update_error: { version: string; error: string; at: number } | null;
};

const bool = (v: unknown) => (typeof v === "boolean" ? v : null);
const strs = (v: unknown, max = MAX_MODELS) => (Array.isArray(v) ? v.filter((x): x is string => typeof x === "string" && validModelId(x)).slice(0, max) : null);
const num = (v: unknown) => (typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : null);

/** A node's reported state, kept to the known fields and sizes (it is node-supplied input). */
export function cleanNodeState(raw: unknown): NodeState | null {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  const r = raw as Record<string, any>;
  const st = (r.settings ?? {}) as Record<string, unknown>;
  const ef = (r.effective ?? {}) as Record<string, unknown>;
  const free = num(r.disk?.free_bytes);
  const total = num(r.disk?.total_bytes);
  const ue = r.update_error;
  return {
    v: Number(r.v) || 1,
    version: typeof r.version === "string" ? r.version.slice(0, 32) : null,
    paused: bool(r.paused),
    settings: { models: strs(st.models), house_models: bool(st.house_models), uncensored: bool(st.uncensored), hosting: bool(st.hosting), storage: bool(st.storage) },
    effective: { served_models: strs(ef.served_models) ?? [], house_models: bool(ef.house_models), uncensored: bool(ef.uncensored), hosting: bool(ef.hosting), storage: bool(ef.storage) },
    disk: free != null && total != null && total > 0 ? { free_bytes: free, total_bytes: total } : null,
    update_error:
      ue && typeof ue === "object" && typeof ue.error === "string"
        ? { version: String(ue.version ?? "").slice(0, 32), error: String(ue.error).slice(0, 500), at: Number(ue.at) || 0 }
        : null,
  };
}

/** Largest state body a node may send (bytes). */
export const MAX_STATE_BYTES = 16_384;

const COMMAND_SCHEMA = [
  `CREATE TABLE IF NOT EXISTS fleet_commands (
     id           TEXT PRIMARY KEY,
     user_id      TEXT NOT NULL,
     node         TEXT NOT NULL,
     peer_id      TEXT NOT NULL,
     action       TEXT NOT NULL,
     args         TEXT NOT NULL DEFAULT '{}',
     status       TEXT NOT NULL,
     created_at   TEXT NOT NULL,
     expires_at   TEXT NOT NULL,
     delivered_at TEXT,
     done_at      TEXT,
     result       TEXT
   )`,
  `CREATE INDEX IF NOT EXISTS idx_fleet_commands_peer ON fleet_commands(peer_id, status)`,
  `CREATE INDEX IF NOT EXISTS idx_fleet_commands_user ON fleet_commands(user_id, created_at)`,
  `CREATE TABLE IF NOT EXISTS fleet_node_state (
     peer_id    TEXT PRIMARY KEY,
     node       TEXT NOT NULL,
     user_id    TEXT NOT NULL,
     state      TEXT NOT NULL,
     updated_at TEXT NOT NULL
   )`,
  `CREATE INDEX IF NOT EXISTS idx_fleet_node_state_user ON fleet_node_state(user_id)`,
];

// ── the site's fleet signing key ──────────────────────────────────────────────

/**
 * An ed25519 signing key from PKCS#8 DER (base64), or a fresh one. Tests pass
 * one in; production keeps its key in the site database (see
 * createFleetService), not in a vault or env file: the team vault refuses
 * signing keys, and a key that changed on every deploy would make every
 * linked node refuse its commands.
 */
export function signingKey(pkcs8: string | undefined): KeyObject {
  if (pkcs8) return createPrivateKey({ key: Buffer.from(pkcs8, "base64"), format: "der", type: "pkcs8" });
  return generateKeyPairSync("ed25519").privateKey;
}

const KEY_SCHEMA = `CREATE TABLE IF NOT EXISTS fleet_signing_key (
     id         INTEGER PRIMARY KEY CHECK (id = 1),
     pkcs8      TEXT NOT NULL,
     created_at TEXT NOT NULL
   )`;

/** The site's one fleet key: made on first use, then read back (the first writer wins a race). */
export async function storedSigningKey(sql: Sql): Promise<KeyObject> {
  await sql.run(KEY_SCHEMA);
  const have = (await sql.all(`SELECT pkcs8 FROM fleet_signing_key WHERE id = 1`))[0];
  if (have) return signingKey(String(have.pkcs8));
  const fresh = (generateKeyPairSync("ed25519").privateKey.export({ format: "der", type: "pkcs8" }) as Buffer).toString("base64");
  await sql.run(`INSERT OR IGNORE INTO fleet_signing_key (id, pkcs8, created_at) VALUES (1, ?, ?)`, [fresh, new Date().toISOString()]);
  const row = (await sql.all(`SELECT pkcs8 FROM fleet_signing_key WHERE id = 1`))[0];
  return signingKey(String(row.pkcs8));
}

/** The raw 32-byte public key, base64: what a node pins at link time. */
export function publicKeyB64(key: KeyObject): string {
  const der = createPublicKey(key).export({ format: "der", type: "spki" }) as Buffer;
  return der.subarray(der.length - 32).toString("base64");
}

/** The exact text the site signs for one command. Must match `command_message` in the CLI's fleet module. */
export function commandMessage(c: { id: string; account_id: string; peer_id: string; action: string; args: string; expires_at: string }): string {
  return `c0mpute fleet command v1\nid: ${c.id}\naccount: ${c.account_id}\nnode: ${c.peer_id}\naction: ${c.action}\nargs: ${c.args}\nexpires: ${c.expires_at}`;
}

/**
 * The text a node signs to fetch its commands (or report one): proves it holds
 * the node key. A poll that carries the node's state adds the state's SHA-256
 * (fleet_agent.rs `poll_message_with_state`), so the state is signed too.
 */
export function pollMessage(o: { site: string; peerId: string; ts: number; kind: "poll" | "result"; id?: string; ok?: boolean; stateSha256?: string }): string {
  const tail = o.kind === "result" ? `\nid: ${o.id}\nok: ${o.ok ? "true" : "false"}` : o.stateSha256 ? `\nstate: ${o.stateSha256}` : "";
  return `c0mpute node ${o.kind} v1\nsite: ${o.site}\nnode: ${o.peerId}\nts: ${o.ts}${tail}`;
}

export const sha256Hex = (s: string) => createHash("sha256").update(s).digest("hex");

// ── the fleet view ────────────────────────────────────────────────────────────

export type FleetNode = {
  node: string;
  peer_id: string | null;
  label: string | null;
  tags: string[];
  verified: boolean;
  via: string;
  online: boolean;
  last_seen_s: number | null;
  version: string | null;
  class: string | null;
  cores: number | null;
  ram_bytes: number | null;
  storage: { free_bytes: number; total_bytes: number } | null;
  info: Record<string, unknown> | null;
  jobs_24h: number;
  earned_usd: { h24: number; d30: number; all: number };
  cost_usd_per_hour: number | null;
  paused: boolean | null;
  pending_commands: number;
  linked_at: string;
  /** What the node last reported (settings, models served, disk, a failed update); null before its first report. */
  node_state: NodeState | null;
  state_at: string | null;
};

export type FleetTotals = { nodes: number; online: number; verified: number; jobs_24h: number; earned_usd: { h24: number; d30: number; all: number } };

type Totals = Map<string, { jobs: number; paid_usd: number }>;

export function buildFleet(o: {
  claims: OperatorNode[];
  meta: NodeMeta[];
  board: NodeRow[];
  t24: Totals;
  t30: Totals;
  tAll: Totals;
  pending?: Map<string, number>;
  paused?: Map<string, boolean>;
  states?: Map<string, { state: NodeState; at: string }>;
}): { nodes: FleetNode[]; totals: FleetTotals } {
  const meta = new Map(o.meta.map((m) => [m.node, m]));
  const board = new Map(o.board.map((n) => [n.id, n]));
  const round = (n: number) => Math.round(n * 1e6) / 1e6;
  const nodes = o.claims.map((c): FleetNode => {
    const m = meta.get(c.node);
    const b = board.get(c.node);
    const seen = b ? Number(b.last_seen_s) : null;
    const jobs24 = b?.jobs_24h ? Number(b.jobs_24h.jobs) || 0 : (o.t24.get(c.node)?.jobs ?? 0);
    return {
      node: c.node,
      peer_id: c.peer_id ?? m?.peer_id ?? null,
      label: c.label ?? b?.name ?? (m?.info?.hostname as string) ?? null,
      tags: m?.tags ?? [],
      verified: !!m,
      via: m ? m.via : c.via,
      online: !!b && (seen == null || seen <= ONLINE_S),
      last_seen_s: seen,
      version: b?.version ?? (m?.info?.version as string) ?? null,
      class: b?.class ?? c.class ?? null,
      cores: b?.cores ?? (m?.info?.cores as number) ?? null,
      ram_bytes: b?.ram_bytes ?? (m?.info?.ram_bytes as number) ?? null,
      storage: b?.storage ? { free_bytes: b.storage.free_bytes, total_bytes: b.storage.total_bytes } : null,
      info: m?.info ?? null,
      jobs_24h: jobs24,
      earned_usd: { h24: round(o.t24.get(c.node)?.paid_usd ?? 0), d30: round(o.t30.get(c.node)?.paid_usd ?? 0), all: round(o.tAll.get(c.node)?.paid_usd ?? 0) },
      cost_usd_per_hour: c.cost_usd_per_hour,
      // The node's own word beats the last pause/resume command we saw finish.
      paused: o.states?.get(c.node)?.state.paused ?? o.paused?.get(c.node) ?? null,
      pending_commands: o.pending?.get(c.node) ?? 0,
      linked_at: m?.verified_at ?? c.created_at,
      node_state: o.states?.get(c.node)?.state ?? null,
      state_at: o.states?.get(c.node)?.at ?? null,
    };
  });
  const sum = (f: (n: FleetNode) => number) => round(nodes.reduce((s, n) => s + f(n), 0));
  return {
    nodes,
    totals: {
      nodes: nodes.length,
      online: nodes.filter((n) => n.online).length,
      verified: nodes.filter((n) => n.verified).length,
      jobs_24h: nodes.reduce((s, n) => s + n.jobs_24h, 0),
      earned_usd: { h24: sum((n) => n.earned_usd.h24), d30: sum((n) => n.earned_usd.d30), all: sum((n) => n.earned_usd.all) },
    },
  };
}

/** Pick nodes by explicit ids (8-char board id or full peer id), by tag, or all. */
export function selectNodes(nodes: FleetNode[], sel: { nodes?: unknown; tag?: unknown; all?: unknown }): FleetNode[] | string {
  if (sel.all === true) return nodes;
  if (typeof sel.tag === "string" && sel.tag) {
    const t = sel.tag.toLowerCase();
    return nodes.filter((n) => n.tags.includes(t));
  }
  const list = Array.isArray(sel.nodes) ? sel.nodes : typeof sel.nodes === "string" ? sel.nodes.split(",") : null;
  if (!list || list.length === 0) return "say which nodes: nodes [ids], tag, or all: true";
  if (list.length > MAX_BULK) return `at most ${MAX_BULK} nodes per action`;
  const out: FleetNode[] = [];
  for (const raw of list) {
    const k = nodeKey(typeof raw === "string" ? raw : "");
    const n = k && nodes.find((x) => x.node === k.node);
    if (!n) return `not one of your nodes: ${String(raw)}`;
    if (!out.includes(n)) out.push(n);
  }
  return out;
}

// ── commands ──────────────────────────────────────────────────────────────────

export type Command = {
  id: string;
  node: string;
  peer_id: string;
  action: string;
  args: string;
  status: "queued" | "delivered" | "done" | "failed" | "expired" | "cancelled";
  created_at: string;
  expires_at: string;
  delivered_at: string | null;
  done_at: string | null;
  result: string | null;
};

const toCommand = (r: Row): Command => ({
  id: String(r.id),
  node: String(r.node),
  peer_id: String(r.peer_id),
  action: String(r.action),
  args: String(r.args ?? "{}"),
  status: String(r.status) as Command["status"],
  created_at: String(r.created_at),
  expires_at: String(r.expires_at),
  delivered_at: (r.delivered_at as string) ?? null,
  done_at: (r.done_at as string) ?? null,
  result: (r.result as string) ?? null,
});

export function createFleetService(deps: { sql: Sql; accounts: AccountStore; site: string; key?: KeyObject; now?: () => number }) {
  const { sql, accounts } = deps;
  let keyP: Promise<KeyObject> | null = deps.key ? Promise.resolve(deps.key) : null;
  const key = () =>
    (keyP ??= storedSigningKey(sql).catch((e) => {
      keyP = null;
      throw e;
    }));
  const clock = deps.now ?? Date.now;
  const iso = (ms = clock()) => new Date(ms).toISOString();
  let ready: Promise<void> | null = null;
  const init = () =>
    (ready ??= (async () => {
      for (const q of COMMAND_SCHEMA) await sql.run(q);
    })().catch((e) => {
      ready = null;
      throw e;
    }));

  const signed = (userId: string, c: Command, k: KeyObject) => {
    const msg = { id: c.id, account_id: userId, peer_id: c.peer_id, action: c.action, args: c.args, expires_at: c.expires_at };
    return { ...msg, issued_at: c.created_at, sig: edSign(null, Buffer.from(commandMessage(msg)), k).toString("base64") };
  };

  return {
    init,
    /** The raw public key (base64) nodes pin when they link. */
    publicKey: async () => publicKeyB64(await key()),

    /** Keep what a node reported about itself (verified poll from a node linked to an account). */
    async saveState(peerId: string, state: NodeState) {
      await init();
      const owner = await accounts.verifiedOwner(nodeKey(peerId)?.node ?? "");
      if (!owner || owner.peer_id !== peerId) return false;
      await sql.run(
        `INSERT INTO fleet_node_state (peer_id, node, user_id, state, updated_at) VALUES (?, ?, ?, ?, ?)
           ON CONFLICT(peer_id) DO UPDATE SET node = excluded.node, user_id = excluded.user_id, state = excluded.state, updated_at = excluded.updated_at`,
        [peerId, nodeKey(peerId)!.node, owner.user_id, JSON.stringify(state), iso()],
      );
      return true;
    },

    /** Queue a node-side action for one verified node. Supersedes the same action still waiting. */
    async enqueue(userId: string, n: { node: string; peer_id: string }, action: NodeAction, args: Record<string, unknown> = {}) {
      await init();
      // pause then resume before the node polled: only the last word counts.
      const opposite = action === "pause" ? "resume" : action === "resume" ? "pause" : null;
      const supersede = [action, ...(opposite ? [opposite] : [])];
      await sql.run(
        `UPDATE fleet_commands SET status = 'cancelled', done_at = ? WHERE user_id = ? AND node = ? AND status = 'queued' AND action IN (${supersede.map(() => "?").join(", ")})`,
        [iso(), userId, n.node, ...supersede],
      );
      const id = "fc_" + randomBytes(12).toString("hex");
      await sql.run(
        `INSERT INTO fleet_commands (id, user_id, node, peer_id, action, args, status, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, 'queued', ?, ?)`,
        [id, userId, n.node, n.peer_id, action, JSON.stringify(args), iso(), iso(clock() + COMMAND_TTL_MS)],
      );
      return id;
    },

    /** Commands for the node with this peer id, signed, marked delivered. The caller checked the node's signature. */
    async forNode(peerId: string) {
      await init();
      const now = iso();
      await sql.run(`UPDATE fleet_commands SET status = 'expired' WHERE peer_id = ? AND status IN ('queued', 'delivered') AND expires_at < ?`, [peerId, now]);
      const owner = await accounts.verifiedOwner(nodeKey(peerId)?.node ?? "");
      // A node that left the account (unlinked, or linked elsewhere) gets nothing queued under the old one.
      if (!owner || owner.peer_id !== peerId) return { account_id: null, commands: [] };
      const rows = (
        await sql.all(
          `SELECT * FROM fleet_commands WHERE peer_id = ? AND user_id = ? AND status IN ('queued', 'delivered') ORDER BY created_at LIMIT 20`,
          [peerId, owner.user_id],
        )
      ).map(toCommand);
      for (const c of rows) if (c.status === "queued") await sql.run(`UPDATE fleet_commands SET status = 'delivered', delivered_at = ? WHERE id = ?`, [now, c.id]);
      const k = await key();
      return { account_id: owner.user_id, commands: rows.map((c) => signed(owner.user_id, c, k)) };
    },

    /** The node reports how a command went. Only the node it was for can report it. */
    async report(peerId: string, id: string, ok: boolean, output: unknown) {
      await init();
      const r = (await sql.all(`SELECT * FROM fleet_commands WHERE id = ?`, [id]))[0];
      if (!r || r.peer_id !== peerId) return false;
      if (r.status === "done" || r.status === "failed") return true;
      const text = (typeof output === "string" ? output : JSON.stringify(output ?? "")).slice(0, 2000);
      await sql.run(`UPDATE fleet_commands SET status = ?, done_at = ?, result = ? WHERE id = ?`, [ok ? "done" : "failed", iso(), text, id]);
      await accounts.audit(String(r.user_id), { node: String(r.node), action: `${r.action}:result`, args: { id }, actor: "node", outcome: `${ok ? "ok" : "failed"}${text ? `: ${text.slice(0, 160)}` : ""}` });
      return true;
    },

    async commands(userId: string, limit = 100): Promise<Command[]> {
      await init();
      return (await sql.all(`SELECT * FROM fleet_commands WHERE user_id = ? ORDER BY created_at DESC LIMIT ?`, [userId, Math.min(Math.max(limit, 1), 500)])).map(toCommand);
    },

    async cancel(userId: string, id: string): Promise<boolean> {
      await init();
      const r = await sql.all(`SELECT id FROM fleet_commands WHERE id = ? AND user_id = ? AND status = 'queued'`, [id, userId]);
      if (!r.length) return false;
      await sql.run(`UPDATE fleet_commands SET status = 'cancelled', done_at = ? WHERE id = ?`, [iso(), id]);
      return true;
    },

    /** Waiting commands per node, each node's last pause/resume that went through, and what each node reported. */
    async state(userId: string): Promise<{ pending: Map<string, number>; paused: Map<string, boolean>; states: Map<string, { state: NodeState; at: string }> }> {
      await init();
      const pending = new Map<string, number>();
      for (const r of await sql.all(`SELECT node, COUNT(*) AS n FROM fleet_commands WHERE user_id = ? AND status IN ('queued', 'delivered') GROUP BY node`, [userId])) {
        pending.set(String(r.node), Number(r.n) || 0);
      }
      const paused = new Map<string, boolean>();
      const rows = await sql.all(
        `SELECT node, action, done_at FROM fleet_commands WHERE user_id = ? AND status = 'done' AND action IN ('pause', 'resume') ORDER BY done_at`,
        [userId],
      );
      for (const r of rows) paused.set(String(r.node), r.action === "pause");
      const states = new Map<string, { state: NodeState; at: string }>();
      for (const r of await sql.all(`SELECT node, state, updated_at FROM fleet_node_state WHERE user_id = ?`, [userId])) {
        try {
          const st = cleanNodeState(JSON.parse(String(r.state)));
          if (st) states.set(String(r.node), { state: st, at: String(r.updated_at) });
        } catch {}
      }
      return { pending, paused, states };
    },

    /** The last update command per node of this account (for the update-failed alert). */
    async lastUpdates(userId: string): Promise<Map<string, { status: string; done_at: string | null; result: string | null }>> {
      await init();
      const out = new Map<string, { status: string; done_at: string | null; result: string | null }>();
      const rows = await sql.all(
        `SELECT node, status, done_at, result FROM fleet_commands WHERE user_id = ? AND action = 'update' AND status IN ('done', 'failed') ORDER BY done_at`,
        [userId],
      );
      for (const r of rows) out.set(String(r.node), { status: String(r.status), done_at: (r.done_at as string) ?? null, result: (r.result as string) ?? null });
      return out;
    },

    /** Forget a node's reported state (unlink). */
    async dropState(userId: string, node: string) {
      await init();
      await sql.run(`DELETE FROM fleet_node_state WHERE user_id = ? AND node = ?`, [userId, node]);
    },
  };
}

export type FleetService = ReturnType<typeof createFleetService>;

/**
 * Apply one action to the selected nodes. Account-side actions change state
 * here; node-side ones are queued for verified nodes and refused for the rest.
 * `unclaim`/`relabel`/`setCost` reach the earnings store (the caller wires them).
 */
export async function runAction(o: {
  userId: string;
  actor: string;
  action: FleetAction;
  args: Record<string, unknown>;
  targets: FleetNode[];
  fleet: FleetService;
  accounts: AccountStore;
  earnings: {
    unclaim(userId: string, node: string): Promise<boolean>;
    update(userId: string, node: string, patch: { label?: string | null; cost_usd_per_hour?: number | null }): Promise<boolean>;
  };
}) {
  const { userId, actor, action, args, targets, fleet, accounts, earnings } = o;
  const results: { node: string; ok: boolean; detail: string; command?: string }[] = [];
  let tags: string[] | null = null;
  if (action === "tag" || action === "untag" || action === "set-tags") {
    tags = cleanTags(args.tags);
    if (!tags || (action !== "set-tags" && tags.length === 0)) return { error: "tags: short lowercase words (a-z 0-9 . _ -), comma separated" };
  }
  if (action === "rename" && targets.length !== 1) return { error: "rename one node at a time" };
  let cmdArgs: Record<string, unknown> | null = null;
  if (isNodeAction(action)) {
    const a = nodeArgs(action, args);
    if (typeof a === "string") return { error: a };
    cmdArgs = a;
  }
  for (const n of targets) {
    let r: { ok: boolean; detail: string; command?: string };
    try {
      if (isNodeAction(action)) {
        if (!n.verified || !n.peer_id) r = { ok: false, detail: "not verified: run `c0mpute node link` on the node first" };
        else r = { ok: true, detail: "queued", command: await fleet.enqueue(userId, { node: n.node, peer_id: n.peer_id }, action, cmdArgs!) };
      } else if (action === "rename") {
        const label = typeof args.label === "string" ? args.label.trim().slice(0, 64) || null : null;
        r = (await earnings.update(userId, n.node, { label })) ? { ok: true, detail: `renamed to ${label ?? "(none)"}` } : { ok: false, detail: "not found" };
      } else if (action === "tag" || action === "untag" || action === "set-tags") {
        if (!n.verified) r = { ok: false, detail: "tags need a verified node: run `c0mpute node link` on it" };
        else {
          const next = await accounts.setTags(userId, n.node, tags!, action === "tag" ? "add" : action === "untag" ? "remove" : "set");
          r = next ? { ok: true, detail: next.join(",") || "(no tags)" } : { ok: false, detail: "not found" };
        }
      } else if (action === "cost") {
        const c = args.cost_usd_per_hour;
        const v = c === null || c === "" ? null : Number(c);
        if (v !== null && (!Number.isFinite(v) || v < 0 || v > 1000)) r = { ok: false, detail: "cost_usd_per_hour: 0-1000 or null" };
        else r = (await earnings.update(userId, n.node, { cost_usd_per_hour: v })) ? { ok: true, detail: `cost ${v ?? "cleared"}` } : { ok: false, detail: "not found" };
      } else {
        // unlink
        const a = await earnings.unclaim(userId, n.node);
        const b = await accounts.unverifyNode(userId, n.node);
        await fleet.dropState(userId, n.node).catch(() => {});
        r = a || b ? { ok: true, detail: "unlinked" } : { ok: false, detail: "not found" };
      }
    } catch (e) {
      r = { ok: false, detail: e instanceof Error ? e.message : "failed" };
    }
    results.push({ node: n.node, ...r });
    await accounts
      .audit(userId, { node: n.node, action, args: action === "rename" || tags ? { ...args } : cmdArgs && Object.keys(cmdArgs).length ? cmdArgs : undefined, actor, outcome: `${r.ok ? "ok" : "refused"}: ${r.detail}` })
      .catch(() => {});
  }
  return { results, ok: results.filter((r) => r.ok).length, failed: results.filter((r) => !r.ok).length };
}
