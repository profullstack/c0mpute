// MCP tools for running a fleet of nodes from an agent: list, earnings,
// actions (single or bulk), and how to link more nodes.

import type { FleetNode, FleetTotals } from "./fleet";

export type FleetMcpDeps = {
  fleet: (userId: string) => Promise<{ nodes: FleetNode[]; totals: FleetTotals }>;
  action: (userId: string, body: { action: string; nodes?: unknown; tag?: unknown; all?: unknown; args?: Record<string, unknown> }) => Promise<unknown>;
  account: (userId: string) => Promise<{ coinpay: boolean; did: string | null; name: string | null; sign_in: unknown }>;
  site: string;
  alerts?: {
    get(userId: string): Promise<unknown>;
    set(userId: string, patch: Record<string, unknown>): Promise<unknown>;
    test(userId: string): Promise<{ ok: boolean; detail: string }>;
  };
};

const sel = {
  nodes: { type: "array", items: { type: "string" }, description: "Node ids (8-char board id or full peer id)" },
  tag: { type: "string", description: "Every node with this tag, e.g. rack-a" },
  all: { type: "boolean", description: "Every node on the account" },
};

export const FLEET_TOOLS = [
  {
    name: "list_my_nodes",
    description: "The signed-in operator's nodes: online/offline, version, hardware, tags, jobs and earnings (24h, 30d, all). Same as fleet_list.",
    inputSchema: { type: "object", properties: { tag: { type: "string" } } },
    annotations: { title: "My nodes", readOnlyHint: true, openWorldHint: false },
  },
  {
    name: "fleet_list",
    description: "Every node on the account with status, last seen, version, hardware, models/jobs, earnings and queued commands, plus fleet totals. Optional tag filter.",
    inputSchema: { type: "object", properties: { tag: { type: "string" } } },
    annotations: { title: "Fleet", readOnlyHint: true, openWorldHint: false },
  },
  {
    name: "my_earnings",
    description: "What the account's nodes earned (buyer-agreed, before the 5% fee) in the last 24h, 30d and all time, per node and in total, and whether payouts are set up (needs CoinPay connected).",
    inputSchema: { type: "object", properties: {} },
    annotations: { title: "My earnings", readOnlyHint: true, openWorldHint: false },
  },
  {
    name: "fleet_earnings",
    description: "Per-node earnings breakdown across the fleet (24h, 30d, all) with totals. Same data as my_earnings.",
    inputSchema: { type: "object", properties: { tag: { type: "string" } } },
    annotations: { title: "Fleet earnings", readOnlyHint: true, openWorldHint: false },
  },
  {
    name: "fleet_action",
    description:
      "Act on one node or many (nodes, tag or all). Node-side, signed and run on the node's next poll (~1 min): pause, resume, update (to the latest release), restart; node settings: models {models: [ids]} (empty list = every model the node has), house-models {on}, uncensored {on}, hosting {on}, storage {on} (on: true, false, or null for the node's own config; hosting/storage restart the worker after running jobs finish). The node reports its new state, shown in fleet_list as node_state. Account-side, immediate: rename {label}, tag/untag/set-tags {tags}, cost {cost_usd_per_hour}, unlink. Node-side actions need nodes linked with `c0mpute node link`. Everything is audit-logged.",
    inputSchema: {
      type: "object",
      properties: {
        action: { type: "string", enum: ["pause", "resume", "update", "restart", "models", "house-models", "uncensored", "hosting", "storage", "rename", "tag", "untag", "set-tags", "cost", "unlink"] },
        ...sel,
        args: { type: "object" },
      },
      required: ["action"],
    },
    annotations: { title: "Fleet action", readOnlyHint: false, destructiveHint: true, openWorldHint: false },
  },
  {
    name: "fleet_alerts_get",
    description:
      "The account's fleet alert settings (email, thresholds, which kinds are on), the alerts open right now and recent notices. Alerts: node offline for N minutes, earnings drop (last 24h vs the 7-day daily average), update failed, disk low.",
    inputSchema: { type: "object", properties: {} },
    annotations: { title: "Fleet alerts", readOnlyHint: true, openWorldHint: false },
  },
  {
    name: "fleet_alerts_set",
    description:
      "Change fleet alert settings. Any of: email (a confirmation link is emailed; null removes it), offline_minutes (5-1440, default 15), earnings_drop_pct (10-100, default 50), earnings_min_usd (default 0.05), disk_min_gb (default 10, 0 = off), disk_min_pct (default 5, 0 = off), cooldown_minutes (default 60), kinds {offline, earnings, update, disk: true|false}, sms (Profullstack admin accounts only). test: true sends a test notice instead.",
    inputSchema: {
      type: "object",
      properties: {
        email: { type: ["string", "null"] },
        offline_minutes: { type: "number" },
        earnings_drop_pct: { type: "number" },
        earnings_min_usd: { type: "number" },
        disk_min_gb: { type: "number" },
        disk_min_pct: { type: "number" },
        cooldown_minutes: { type: "number" },
        kinds: { type: "object", properties: { offline: { type: "boolean" }, earnings: { type: "boolean" }, update: { type: "boolean" }, disk: { type: "boolean" } } },
        sms: { type: "boolean" },
        test: { type: "boolean" },
      },
    },
    annotations: { title: "Set fleet alerts", readOnlyHint: false, destructiveHint: false, openWorldHint: true },
  },
  {
    name: "link_node",
    description: "How to link a node to this account: on the node run `c0mpute login` then `c0mpute node link`, or headless with an enrollment token (create one on /account or `c0mpute fleet token create`).",
    inputSchema: { type: "object", properties: {} },
    annotations: { title: "Link a node", readOnlyHint: true, openWorldHint: false },
  },
] as const;

export const FLEET_TOOL_NAMES = new Set<string>(FLEET_TOOLS.map((t) => t.name));

const usd = (n: number) => `$${n.toFixed(4)}`;

export async function fleetTool(name: string, args: Record<string, unknown>, userId: string, deps: FleetMcpDeps): Promise<{ text: string; isError?: boolean }> {
  if (name === "link_node") {
    return {
      text: [
        "Link a node you run (it proves control by signing a challenge with its identity key):",
        "  c0mpute login            # once per machine: approve in the browser (CoinPay, passkey or password)",
        "  c0mpute node link        # signs and links this node",
        "Many boxes without a browser: create an enrollment token on " + deps.site + "/account (Fleet tokens) or `c0mpute fleet token create --name rack-a --expires 7d`, then on each box:",
        `  curl -fsSL ${deps.site}/install.sh | sh -s -- --enroll c0e_...`,
        "  # or, already installed: c0mpute node link --token c0e_...",
      ].join("\n"),
    };
  }
  if (name === "fleet_alerts_get" || name === "fleet_alerts_set") {
    if (!deps.alerts) return { text: "alerts are not available here", isError: true };
    if (name === "fleet_alerts_get") return { text: JSON.stringify(await deps.alerts.get(userId)) };
    if (args.test === true) {
      const r = await deps.alerts.test(userId);
      return { text: r.detail, isError: !r.ok };
    }
    const { test: _t, ...patch } = args;
    void _t;
    const r = await deps.alerts.set(userId, patch);
    const err = r && typeof r === "object" && "error" in r ? String((r as { error: unknown }).error) : null;
    return err ? { text: err, isError: true } : { text: JSON.stringify(r) };
  }
  if (name === "fleet_action") {
    const action = typeof args.action === "string" ? args.action : "";
    const r = await deps.action(userId, { action, nodes: args.nodes, tag: args.tag, all: args.all, args: (args.args as Record<string, unknown>) ?? {} });
    const err = r && typeof r === "object" && "error" in r ? String((r as { error: unknown }).error) : null;
    return err ? { text: err, isError: true } : { text: JSON.stringify(r) };
  }
  const f = await deps.fleet(userId);
  const tag = typeof args.tag === "string" && args.tag ? args.tag.toLowerCase() : null;
  const nodes = tag ? f.nodes.filter((n) => n.tags.includes(tag)) : f.nodes;
  if (name === "list_my_nodes" || name === "fleet_list") {
    if (!nodes.length) return { text: tag ? `no nodes tagged ${tag}` : "no nodes on this account yet; call link_node for how to add one" };
    return {
      text: JSON.stringify({
        totals: f.totals,
        nodes: nodes.map((n) => ({
          node: n.node,
          label: n.label,
          online: n.online,
          last_seen_s: n.last_seen_s,
          verified: n.verified,
          tags: n.tags,
          version: n.version,
          class: n.class,
          cores: n.cores,
          ram_gb: n.ram_bytes ? Math.round(n.ram_bytes / 1e9) : null,
          jobs_24h: n.jobs_24h,
          earned_usd: n.earned_usd,
          paused: n.paused,
          pending_commands: n.pending_commands,
          node_state: n.node_state,
          state_at: n.state_at,
        })),
      }),
    };
  }
  // my_earnings / fleet_earnings
  const acct = await deps.account(userId);
  const lines = nodes.map((n) => `${n.node}  ${(n.label ?? "").padEnd(18).slice(0, 18)}  24h ${usd(n.earned_usd.h24)}  30d ${usd(n.earned_usd.d30)}  all ${usd(n.earned_usd.all)}`);
  return {
    text: [
      `fleet total: 24h ${usd(f.totals.earned_usd.h24)}  30d ${usd(f.totals.earned_usd.d30)}  all ${usd(f.totals.earned_usd.all)} (buyer-agreed, before the 5% fee)`,
      acct.coinpay ? "payouts: to your CoinPay account" : "payouts: not set up. Connect CoinPay at " + deps.site + "/account to get paid.",
      ...lines,
    ].join("\n"),
  };
}
