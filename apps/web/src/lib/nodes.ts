import "server-only";

/** One row of the public node board, as served by the aggregator's `GET /nodes`. */
export interface NodeRow {
  id: string;
  class: string;
  cores: number | null;
  bench_score: number | null;
  version: string | null;
  ref: string | null;
  last_seen_s: number;
}

export interface NodesPayload {
  generated_at: string;
  workers_online: number;
  nodes: NodeRow[];
  referrers: { ref: string; nodes: number }[];
  source: "aggregator" | "unavailable";
}

/** The mission number. A goal, not a count. */
export const NODE_GOAL = 100_000;

const AGGREGATOR_URL = process.env.STATUS_AGGREGATOR_URL ?? null;

export async function getNodesPayload(): Promise<NodesPayload> {
  if (AGGREGATOR_URL) {
    try {
      const r = await fetch(`${AGGREGATOR_URL.replace(/\/$/, "")}/nodes`, {
        signal: AbortSignal.timeout(2_000),
        cache: "no-store",
      });
      if (r.ok) {
        const data = (await r.json()) as Omit<NodesPayload, "source">;
        return { ...data, source: "aggregator" };
      }
    } catch {
      // fall through
    }
  }
  return {
    generated_at: new Date().toISOString(),
    workers_online: 0,
    nodes: [],
    referrers: [],
    source: "unavailable",
  };
}

/** A referral code as the worker accepts it: lowercase [a-z0-9_-], 1 to 32 chars. */
export function sanitizeRef(raw: string | null | undefined): string | null {
  if (!raw) return null;
  const code = raw.trim().toLowerCase();
  return /^[a-z0-9_-]{1,32}$/.test(code) ? code : null;
}

export function installLine(ref?: string | null): string {
  const code = sanitizeRef(ref);
  return code
    ? `curl -fsSL https://c0mpute.com/install.sh | C0MPUTE_REF=${code} sh`
    : "curl -fsSL https://c0mpute.com/install.sh | sh";
}
