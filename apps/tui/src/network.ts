/**
 * The network as c0mpute.com shows it: /api/status (workers online, per-role
 * counts, jobs in flight), /api/nodes (the public node board), Infernet's
 * /api/v1/pricing (live per-model prices), and for a signed-in person
 * /api/v1/me (name, DID, 18+ verification, balance) and /api/v1/models.
 */
import { Api, ApiError } from "./api.ts";
import { INFERNET_URL } from "./config.ts";

export interface WorkloadStats {
  jobs_in_flight?: number;
  jobs_completed_24h?: number;
  avg_latency_seconds?: number | null;
}

export interface StatusPayload {
  ok?: boolean;
  generated_at?: string;
  network: {
    workers_online: number;
    workers_with_role: Record<string, number>;
    workers_with_tag?: Record<string, number>;
    jobs_in_flight?: number;
    jobs_completed_24h?: number;
    avg_job_latency_seconds?: number | null;
    workload_types?: Record<string, WorkloadStats>;
  };
}

export interface NodeRow {
  id: string;
  class: string;
  cores: number | null;
  ram_bytes: number | null;
  bench_score: number | null;
  version: string | null;
  name?: string | null;
  pricing?: { mode: "dynamic" | "fixed"; min_usd: number; fixed_usd?: number } | null;
  gateway?: string | null;
  donates?: string[];
  last_seen_s: number;
  duplicate?: { instances: number } | null;
}

export interface NodesPayload {
  generated_at: string;
  workers_online: number;
  nodes: NodeRow[];
  operators?: { name: string; nodes: number }[];
  source?: string;
}

export interface ModelPrice {
  model: string;
  input_per_m_usd: number;
  output_per_m_usd: number;
  live?: boolean;
  source?: string;
  market?: { multiplier?: number; reason?: string; providers?: number; busy?: number; scarce?: boolean };
}

export interface PricingPayload {
  unit?: string;
  default_model_price?: { input_per_m_usd: number; output_per_m_usd: number };
  models: ModelPrice[];
}

export interface Balance {
  spendable_usd: string;
  earned_spendable_usd: string;
  prepaid_available_usd: string;
}

export interface Me {
  id: string;
  did: string;
  name: string | null;
  auth?: string;
  adult_verified_at: string | null;
  balance: Balance | null;
}

export interface Loadable<T> {
  data: T | null;
  error: string | null;
  at: number | null;
  loading: boolean;
}

export const idle = <T>(): Loadable<T> => ({ data: null, error: null, at: null, loading: false });

/** Load into a Loadable, keeping the last good data on failure (stale beats blank). */
export async function load<T>(prev: Loadable<T>, fn: () => Promise<T>, now = Date.now): Promise<Loadable<T>> {
  try {
    return { data: await fn(), error: null, at: now(), loading: false };
  } catch (e) {
    return { ...prev, error: e instanceof Error ? e.message : String(e), loading: false };
  }
}

async function ok<T>(p: Promise<{ status: number; body: T | null }>, what: string): Promise<T> {
  const r = await p;
  if (r.status !== 200 || r.body === null) throw new ApiError(`${what}: HTTP ${r.status}`, "http", r.status);
  return r.body;
}

export const fetchStatus = (api: Api) => ok(api.json<StatusPayload>("/api/status", { auth: false, timeoutMs: 15_000 }), "network status");
export const fetchNodes = (api: Api) => ok(api.json<NodesPayload>("/api/nodes", { auth: false, timeoutMs: 15_000 }), "node list");
export const fetchPricing = (api: Api) =>
  ok(api.json<PricingPayload>(`${process.env.INFERNET_URL || INFERNET_URL}/api/v1/pricing`, { auth: false, timeoutMs: 15_000 }), "prices");
export const fetchMe = (api: Api) => ok(api.json<Me>("/api/v1/me", { timeoutMs: 15_000 }), "account");

export async function fetchModels(api: Api): Promise<string[]> {
  const body = await ok(api.json<{ data?: { id: string }[] }>("/api/v1/models", { auth: false, timeoutMs: 15_000 }), "models");
  return (body.data ?? []).map((m) => m.id).filter((id) => typeof id === "string");
}

/** Roles sorted by how many workers serve them, busiest first. */
export function roleCounts(status: StatusPayload | null): [string, number][] {
  return Object.entries(status?.network.workers_with_role ?? {}).sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
}

/** "dynamic · min $0.02" / "fixed $0.30" / "-", like the market page. */
export function priceLabel(p: NodeRow["pricing"]): string {
  if (!p) return "-";
  const usd = (n: number) => `$${n < 0.01 && n > 0 ? n : n.toFixed(2)}`;
  if (p.mode === "fixed" && p.fixed_usd != null) return `fixed ${usd(Math.max(p.fixed_usd, p.min_usd))}`;
  return p.min_usd > 0 ? `dyn · min ${usd(p.min_usd)}` : "dynamic";
}

export function gib(bytes: number | null | undefined): string {
  return bytes ? `${Math.round(bytes / 2 ** 30)}G` : "-";
}

export function ago(seconds: number): string {
  if (seconds < 60) return `${Math.round(seconds)}s`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)}h`;
  return `${Math.round(seconds / 86400)}d`;
}

export function usd(v: string | number | null | undefined): string {
  const n = Number(v ?? 0);
  if (!Number.isFinite(n)) return "$0.00";
  return n > 0 && n < 0.01 ? `$${n.toFixed(6)}` : `$${n.toFixed(2)}`;
}
