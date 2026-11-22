// Server half of /leaderboard and /api/leaderboard: reads every source once,
// builds the anonymous payload (./build) and keeps it 5 minutes. A page view
// never runs a query; at most one rebuild runs at a time, and a stale copy is
// served while it does.
import "server-only";
import { cloudSql, env, infernet } from "../chat/server";
import { getNodesPayload } from "../nodes";
import { getStatusPayload } from "../status";
import { earningsStore } from "../earnings/recorder";
import { getNetworkEarnings } from "../earnings/network-server";
import { SETTLED_IN_INFERNET_TYPES } from "../earnings/network";
import { leaderboardSecret, personToken, piiFindings, scrubPii } from "./anon";
import { WINDOW_KEYS, WINDOW_MS, buildLeaderboard, type InfernetWindow, type LeaderboardPayload, type UsageRow, type Window } from "./build";
import { createLeaderboardStore, nodeWindowStats, type LeaderboardStore } from "./store";

let _store: LeaderboardStore | null = null;
export const leaderboardStore = () => (_store ??= createLeaderboardStore(cloudSql));

/** Count one call for the leaderboard. Never awaited on a request path; never throws. */
export function recordUsage(userId: string | null | undefined, surface: "chat" | "api" | "mcp", item: unknown): void {
  if (!userId) return;
  leaderboardStore()
    .record(userId, surface, item)
    .catch(() => {});
}

const TTL_MS = 5 * 60_000;
let cache: { at: number; value: LeaderboardPayload } | null = null;
let inflight: Promise<LeaderboardPayload> | null = null;

const ZERO_TOTALS: InfernetWindow["totals"] = { spenders: 0, spent_usd: 0, billed_jobs: 0, earning_providers: 0, served_jobs: 0, output_tokens: 0 };

const since = (w: Window, now: number) => (w === "all" ? 0 : now - WINDOW_MS[w]);

async function readInfernet(): Promise<Record<Window, InfernetWindow> | null> {
  try {
    const r = await infernet("/api/v1/network/leaderboard", { signal: AbortSignal.timeout(20_000), cache: "no-store" });
    if (!r.ok) return null;
    const body = (await r.json()) as { data?: { windows?: Record<string, InfernetWindow> } };
    const w = body.data?.windows;
    if (!w || typeof w !== "object") return null;
    const out = {} as Record<Window, InfernetWindow>;
    for (const k of WINDOW_KEYS) {
      const x = w[k];
      if (!x || typeof x !== "object") return null;
      out[k] = {
        providers: Array.isArray(x.providers) ? x.providers : [],
        spenders: Array.isArray(x.spenders) ? x.spenders : [],
        referrers: Array.isArray(x.referrers) ? x.referrers : [],
        models: Array.isArray(x.models) ? x.models : [],
        totals: { ...ZERO_TOTALS, ...((x.totals ?? {}) as Partial<InfernetWindow["totals"]>) },
      };
    }
    return out;
  } catch {
    return null;
  }
}

async function build(now: number): Promise<LeaderboardPayload> {
  const clientKey = env("INFERNET_CLIENT_KEY");
  const secret = leaderboardSecret({ LEADERBOARD_SECRET: env("LEADERBOARD_SECRET"), INFERNET_CLIENT_KEY: clientKey });
  const store = leaderboardStore();
  const tokenOf = (sub: string) => (clientKey ? personToken(clientKey, sub) : null);

  const nodeStatsP = (async () => {
    await earningsStore().init();
    const out = {} as Record<Window, Awaited<ReturnType<typeof nodeWindowStats>>>;
    for (const w of WINDOW_KEYS) out[w] = await nodeWindowStats(cloudSql, since(w, now), SETTLED_IN_INFERNET_TYPES);
    return out;
  })().catch(() => Object.fromEntries(WINDOW_KEYS.map((w) => [w, []])) as unknown as Record<Window, Awaited<ReturnType<typeof nodeWindowStats>>>);

  const usageP = (async () => {
    const out = {} as Record<Window, UsageRow[]>;
    for (const w of WINDOW_KEYS) {
      out[w] = (await store.usageSince(since(w, now))).flatMap((u) => {
        const token = tokenOf(u.sub);
        return token ? [{ token, surface: u.surface, item: u.item, calls: u.calls }] : [];
      });
    }
    return out;
  })().catch(() => Object.fromEntries(WINDOW_KEYS.map((w) => [w, []])) as unknown as Record<Window, UsageRow[]>);

  const [nodes, status, nodeStats, usage, infernetBoards, earnings, hidden, usageFrom, jobsFrom] = await Promise.all([
    getNodesPayload(),
    getStatusPayload().catch(() => null),
    nodeStatsP,
    usageP,
    readInfernet(),
    getNetworkEarnings(now).catch(() => null),
    store.hidden().catch(() => ({ subs: [] as string[], nodes: [] as string[] })),
    store.firstDay().catch(() => null),
    earningsStore().firstJobAt().catch(() => null),
  ]);

  const paid =
    earnings && (earnings.c0mpute || earnings.infernet)
      ? (Object.fromEntries(
          WINDOW_KEYS.map((w) => [w, Math.round(((earnings.c0mpute?.[w]?.earned_usd ?? 0) + (earnings.infernet?.[w]?.earned_usd ?? 0)) * 1e6) / 1e6]),
        ) as Record<Window, number>)
      : null;

  const payload = buildLeaderboard({
    generatedAt: new Date(now).toISOString(),
    secret,
    workersOnline: Math.max(nodes.workers_online ?? 0, status?.network?.workers_online ?? 0),
    nodes: nodes.nodes.map((n) => ({ id: n.id, class: n.class, bench_score: n.bench_score, name: n.name ?? null, last_seen_s: n.last_seen_s })),
    referralHandles: nodes.referrers,
    nodeStats,
    infernet: clientKey ? infernetBoards : null,
    usage,
    paidToOperators: paid,
    hiddenTokens: new Set(hidden.subs.map(tokenOf).filter((t): t is string => !!t)),
    hiddenNodes: new Set(hidden.nodes),
    history: { node_jobs_from: jobsFrom ? new Date(jobsFrom).toISOString().slice(0, 10) : null, usage_from: usageFrom },
  });

  // Last line of defence: nothing that looks like PII ships, whatever a source
  // sent. A forbidden field means the builder is wrong: refuse to publish.
  const keys = piiFindings(payload).filter((f) => f.startsWith("key "));
  if (keys.length) {
    console.error("leaderboard: refusing to publish, forbidden fields:", keys.slice(0, 5));
    throw new Error("leaderboard failed its privacy check");
  }
  const { value, scrubbed } = scrubPii(payload);
  if (scrubbed) console.warn(`leaderboard: hid ${scrubbed} PII-like value(s)`);
  return value;
}

/** The leaderboard, at most 5 minutes old. */
export async function getLeaderboard(now = Date.now()): Promise<LeaderboardPayload | null> {
  if (cache && now - cache.at < TTL_MS) return cache.value;
  if (!inflight) {
    inflight = build(now)
      .then((value) => {
        cache = { at: Date.now(), value };
        return value;
      })
      .finally(() => {
        inflight = null;
      });
  }
  if (cache) {
    inflight.catch(() => {});
    return cache.value;
  }
  return inflight.catch((e) => {
    console.error("leaderboard build failed", e);
    return null;
  });
}
