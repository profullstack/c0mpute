/**
 * The account's fleet: every node linked to it (GET /api/v1/fleet) and the
 * actions that can be sent to some of them (POST /api/v1/fleet/actions).
 *
 * Everything here is pure (selection, confirmation text, request bodies,
 * reading the results) so the tests pin the exact wire format; the
 * controller only sequences it.
 */
import { Api, ApiError, errorMessage } from "./api.ts";

export interface NodeSettings {
  models: string[] | null;
  house_models: boolean | null;
  uncensored: boolean | null;
  hosting: boolean | null;
  storage: boolean | null;
}

export interface NodeEffective {
  served_models: string[];
  house_models: boolean;
  uncensored: boolean;
  hosting: boolean;
  storage: boolean;
}

/** What a node last reported about itself (null until it reports once). */
export interface NodeState {
  v: number;
  version: string | null;
  paused: boolean | null;
  settings: NodeSettings;
  effective: NodeEffective;
  disk: { free_bytes: number; total_bytes: number } | null;
  update_error: { version: string; error: string; at: string } | null;
}

export interface FleetNode {
  node: string;
  label: string | null;
  online: boolean;
  last_seen_s: number | null;
  version: string | null;
  tags: string[];
  verified: boolean;
  paused: boolean | null;
  pending_commands: number;
  jobs_24h: number;
  earned_usd: { h24: number | string; d30: number | string; all: number | string };
  node_state?: NodeState | null;
  state_at?: string | null;
}

export interface FleetPayload {
  nodes: FleetNode[];
  totals?: Record<string, unknown>;
  fleet_key?: string | null;
  payouts_ready?: boolean;
}

export type ToggleAction = "house-models" | "uncensored" | "hosting" | "storage";
export type TextAction = "tag" | "untag" | "models";
export type PlainAction = "pause" | "resume" | "update" | "restart";
export type FleetAction = PlainAction | ToggleAction | TextAction;

export const TOGGLE_ACTIONS: ToggleAction[] = ["house-models", "uncensored", "hosting", "storage"];

/** Which effective field a toggle flips. */
export const TOGGLE_FIELD: Record<ToggleAction, keyof Omit<NodeEffective, "served_models">> = {
  "house-models": "house_models",
  uncensored: "uncensored",
  hosting: "hosting",
  storage: "storage",
};

const TOGGLE_LABEL: Record<ToggleAction, string> = {
  "house-models": "house models",
  uncensored: "uncensored models",
  hosting: "hosting",
  storage: "storage",
};

/** Every action, its key on the Fleet tab and what it does (the action menu and the help). */
export const FLEET_ACTIONS: { action: FleetAction; key: string; label: string }[] = [
  { action: "pause", key: "p", label: "pause (stop taking jobs)" },
  { action: "resume", key: "c", label: "resume (continue taking jobs)" },
  { action: "update", key: "U", label: "update to the latest c0mpute" },
  { action: "restart", key: "X", label: "restart the worker" },
  { action: "tag", key: "t", label: "add tags" },
  { action: "untag", key: "T", label: "remove tags" },
  { action: "models", key: "m", label: "models to serve" },
  { action: "house-models", key: "H", label: "house models on/off" },
  { action: "uncensored", key: "u", label: "uncensored models on/off" },
  { action: "hosting", key: "o", label: "hosting on/off" },
  { action: "storage", key: "s", label: "storage on/off" },
];

/** Actions the node itself carries out (on its next poll), as opposed to account-side tags. */
export const isNodeSide = (a: FleetAction) => a !== "tag" && a !== "untag";

export interface FleetRequest {
  action: FleetAction;
  nodes: string[];
  args: Record<string, unknown>;
}

export interface FleetResult {
  node: string;
  ok: boolean;
  detail: string;
}

export interface FleetOutcome {
  action: FleetAction;
  results: FleetResult[];
  ok: number;
  failed: number;
  /** HTTP 409: every target refused. */
  refused: boolean;
  nodeSide: boolean;
}

// ─── listing ───────────────────────────────────────────────────────────

/** Online first, then by label (or id): a stable order for the list and the selection. */
export function sortFleet(nodes: FleetNode[]): FleetNode[] {
  return nodes.slice().sort((a, b) => Number(b.online) - Number(a.online) || (a.label ?? a.node).localeCompare(b.label ?? b.node) || a.node.localeCompare(b.node));
}

export async function fetchFleet(api: Api): Promise<FleetPayload> {
  const r = await api.json<FleetPayload>("/api/v1/fleet", { timeoutMs: 15_000 });
  if (r.status !== 200 || !r.body) throw new ApiError(`fleet: ${errorMessage(r.body, r.status)}`, "http", r.status);
  return { ...r.body, nodes: sortFleet(Array.isArray(r.body.nodes) ? r.body.nodes : []) };
}

const onOff = (v: boolean | null | undefined) => (v === null || v === undefined ? "?" : v ? "on" : "off");

export function bytesLabel(n: number): string {
  if (n >= 2 ** 40) return `${(n / 2 ** 40).toFixed(1)}T`;
  if (n >= 2 ** 30) return `${Math.round(n / 2 ** 30)}G`;
  return `${Math.round(n / 2 ** 20)}M`;
}

const money = (v: number | string | null | undefined) => {
  const n = Number(v ?? 0);
  return Number.isFinite(n) ? `$${n.toFixed(2)}` : "$0.00";
};

/** One row of the fleet table (pure; tested). */
export function fleetRow(n: FleetNode, picked: boolean) {
  const st = n.node_state ?? null;
  const paused = n.paused ?? st?.paused ?? null;
  return {
    pick: picked ? "■" : " ",
    dot: n.online ? "●" : "○",
    node: n.node,
    label: n.label ?? "",
    version: n.version ?? st?.version ?? "-",
    state: paused ? "paused" : n.online ? "running" : "offline",
    tags: n.tags.join(","),
    jobs: String(n.jobs_24h ?? 0),
    earned: `${money(n.earned_usd?.h24)}/${money(n.earned_usd?.d30)}`,
    online: n.online,
    paused: !!paused,
  };
}

/** The detail lines for one node, incl. what it last reported (pure; tested). */
export function nodeDetail(n: FleetNode): string[] {
  const st = n.node_state ?? null;
  const lines = [
    `${n.online ? "● online" : "○ offline"}${n.last_seen_s !== null && n.last_seen_s !== undefined ? ` · seen ${ago(n.last_seen_s)} ago` : ""} · ${n.verified ? "verified" : "unverified"}${n.pending_commands ? ` · ${n.pending_commands} command(s) waiting for the next poll` : ""}`,
    `earned 24h ${money(n.earned_usd?.h24)} · 30d ${money(n.earned_usd?.d30)} · all ${money(n.earned_usd?.all)} · ${n.jobs_24h ?? 0} jobs 24h`,
  ];
  if (!st) {
    lines.push("no state reported yet: models, toggles and disk show once the node runs a c0mpute that reports them");
    return lines;
  }
  const e = st.effective;
  const s = st.settings;
  lines.push(
    `serves ${e.served_models.length} model(s)${s.models && s.models.length ? ` (limited to: ${s.models.join(", ")})` : " (every model it has)"}`,
    `house ${onOff(e.house_models)} · uncensored ${onOff(e.uncensored)} · hosting ${onOff(e.hosting)} · storage ${onOff(e.storage)}`,
    st.disk ? `disk ${bytesLabel(st.disk.free_bytes)} free of ${bytesLabel(st.disk.total_bytes)}` : "disk: not reported",
  );
  if (e.served_models.length) lines.push(`models: ${e.served_models.join(", ")}`);
  if (st.update_error) lines.push(`update to ${st.update_error.version} failed: ${st.update_error.error}`);
  if (n.state_at) lines.push(`reported ${n.state_at}`);
  return lines;
}

function ago(seconds: number): string {
  if (seconds < 60) return `${Math.round(seconds)}s`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)}h`;
  return `${Math.round(seconds / 86400)}d`;
}

// ─── selection ─────────────────────────────────────────────────────────

/** Space: add or remove a node from the selection. */
export function togglePick(picked: string[], id: string): string[] {
  return picked.includes(id) ? picked.filter((p) => p !== id) : [...picked, id];
}

/** a: select every node, or clear when all are already selected. */
export function pickAll(nodes: FleetNode[], picked: string[]): string[] {
  return nodes.length && nodes.every((n) => picked.includes(n.node)) ? [] : nodes.map((n) => n.node);
}

/** What an action applies to: the selection (in list order), else the highlighted node. */
export function targets(nodes: FleetNode[], sel: number, picked: string[]): string[] {
  const chosen = nodes.filter((n) => picked.includes(n.node)).map((n) => n.node);
  if (chosen.length) return chosen;
  const n = nodes[sel];
  return n ? [n.node] : [];
}

// ─── requests ──────────────────────────────────────────────────────────

/** "a,b" with spaces and empties dropped. */
export const cleanTags = (text: string) =>
  text
    .split(",")
    .map((t) => t.trim())
    .filter(Boolean)
    .join(",");

export const parseModels = (text: string) =>
  text
    .split(",")
    .map((t) => t.trim())
    .filter(Boolean);

/** The exact POST body (pure; tested). */
export function buildRequest(action: FleetAction, nodes: string[], value?: boolean | string | string[]): FleetRequest {
  if ((TOGGLE_ACTIONS as string[]).includes(action)) return { action, nodes, args: { on: !!value } };
  if (action === "tag" || action === "untag") return { action, nodes, args: { tags: typeof value === "string" ? cleanTags(value) : "" } };
  if (action === "models") return { action, nodes, args: { models: Array.isArray(value) ? value : typeof value === "string" ? parseModels(value) : [] } };
  return { action, nodes, args: {} };
}

/** The value a toggle sets: the opposite of the highlighted node's effective value (on when unknown). */
export function flipValue(node: FleetNode | undefined, action: ToggleAction): boolean {
  const cur = node?.node_state?.effective?.[TOGGLE_FIELD[action]];
  return typeof cur === "boolean" ? !cur : true;
}

function nodeList(ids: string[]): string {
  const shown = ids.slice(0, 4).join(", ");
  return ids.length > 4 ? `${shown}, +${ids.length - 4} more` : shown;
}

/** "Pause 3 nodes (a1b2c3d4, ...)? y/N" (pure; tested). */
export function confirmText(req: FleetRequest): string {
  const n = req.nodes.length;
  const who = `${n} node${n === 1 ? "" : "s"} (${nodeList(req.nodes)})`;
  const a = req.action;
  let what: string;
  if ((TOGGLE_ACTIONS as string[]).includes(a)) what = `Turn ${TOGGLE_LABEL[a as ToggleAction]} ${req.args.on ? "ON" : "OFF"} on ${who}`;
  else if (a === "tag") what = `Add tags "${req.args.tags}" to ${who}`;
  else if (a === "untag") what = `Remove tags "${req.args.tags}" from ${who}`;
  else if (a === "models") {
    const m = req.args.models as string[];
    what = m.length ? `Serve only ${m.join(", ")} on ${who}` : `Serve every model each node has on ${who}`;
  } else if (a === "update") what = `Update ${who} to the latest c0mpute`;
  else if (a === "restart") what = `Restart the worker on ${who}`;
  else what = `${a[0]!.toUpperCase()}${a.slice(1)} ${who}`;
  return `${what}? y/N`;
}

/** Read a POST /api/v1/fleet/actions response; 409 with results = every target refused. */
export function readOutcome(req: FleetRequest, status: number, body: unknown): FleetOutcome {
  const b = (body ?? {}) as { results?: unknown; ok?: unknown; failed?: unknown };
  const results: FleetResult[] = Array.isArray(b.results)
    ? b.results.map((r: { node?: unknown; ok?: unknown; detail?: unknown }) => ({ node: String(r?.node ?? "?"), ok: r?.ok === true, detail: typeof r?.detail === "string" ? r.detail : "" }))
    : [];
  if (status === 409 && results.length) {
    return { action: req.action, results, ok: 0, failed: results.length, refused: true, nodeSide: isNodeSide(req.action) };
  }
  if (status !== 200) throw new ApiError(errorMessage(body, status), "http", status);
  const ok = typeof b.ok === "number" ? b.ok : results.filter((r) => r.ok).length;
  const failed = typeof b.failed === "number" ? b.failed : results.filter((r) => !r.ok).length;
  return { action: req.action, results, ok, failed, refused: false, nodeSide: isNodeSide(req.action) };
}

export async function postAction(api: Api, req: FleetRequest): Promise<FleetOutcome> {
  const r = await api.json("/api/v1/fleet/actions", { method: "POST", body: req, timeoutMs: 20_000 });
  return readOutcome(req, r.status, r.body);
}
