/**
 * Dashboard state and the pure functions that derive it. Nothing here touches
 * the terminal, so every piece is unit-testable; `loadState` is the only
 * function that reads the filesystem.
 */
import { readFileSync } from "node:fs";
import { benchPath, workerPidPath, type Env } from "./paths.ts";

// ─── bench.json (written by `c0mpute bench`, see node/crates/c0mpute-bench) ──

export interface BenchSample {
  params: string;
  threads: number;
  result: {
    duration: string;
    duration_us: number;
    scaled: number;
    speedup: number;
    throughput: number;
  };
}

export interface BenchReport {
  metadata: {
    start_time: string;
    cpu: string;
    cores: number;
    kernel: string;
    compiler: string;
    version: string;
    quick: boolean;
    elapsed_ms: number;
  };
  /** runtime → workload → samples */
  results: Record<string, Record<string, BenchSample[]>>;
  score: number;
}

export const RUNTIME = "c0mpute";
export const WORKLOADS = ["fib", "matmul", "hash"] as const;
export type Workload = (typeof WORKLOADS)[number];

export interface WorkloadSummary {
  name: Workload;
  params: string;
  /** Best (fastest) sample. */
  best: BenchSample;
  /** speedup / threads at the highest thread count, 0..1. */
  efficiency: number;
  /** Speedup per thread count, in ladder order, for the sparkline. */
  speedups: number[];
}

export function summarizeWorkloads(report: BenchReport): WorkloadSummary[] {
  const ours = report.results[RUNTIME] ?? {};
  const out: WorkloadSummary[] = [];
  for (const name of WORKLOADS) {
    const samples = ours[name];
    if (!samples || samples.length === 0) continue;
    const best = samples.reduce((a, b) => (b.result.duration_us < a.result.duration_us ? b : a));
    const widest = samples.reduce((a, b) => (b.threads > a.threads ? b : a));
    out.push({
      name,
      params: samples[0]!.params,
      best,
      efficiency: Math.min(1, Math.max(0, widest.result.speedup / Math.max(1, widest.threads))),
      speedups: samples.map((s) => s.result.speedup),
    });
  }
  return out;
}

/** `1.2 Gvis/s`, `3.4 GFLOP/s`, `2.1 GiB/s` — same wording as the CLI. */
export function humanRate(workload: string, perSec: number): string {
  if (workload === "hash") {
    const gib = perSec / 2 ** 30;
    return gib >= 1 ? `${gib.toFixed(2)} GiB/s` : `${(perSec / 2 ** 20).toFixed(0)} MiB/s`;
  }
  if (workload === "matmul") return `${(perSec / 1e9).toFixed(2)} GFLOP/s`;
  return perSec >= 1e9 ? `${(perSec / 1e9).toFixed(2)} Gvis/s` : `${(perSec / 1e6).toFixed(0)} Mvis/s`;
}

export function parseBenchReport(json: string): BenchReport | null {
  try {
    const data = JSON.parse(json) as Partial<BenchReport>;
    if (!data || typeof data !== "object" || !data.metadata || !data.results || typeof data.score !== "number") {
      return null;
    }
    return data as BenchReport;
  } catch {
    return null;
  }
}

// ─── worker ─────────────────────────────────────────────────────────────

export type WorkerStatus = { kind: "running"; pid: number } | { kind: "stale"; pid: number } | { kind: "stopped" };

export function workerStatusFrom(pidText: string | null, alive: (pid: number) => boolean): WorkerStatus {
  if (pidText === null) return { kind: "stopped" };
  const pid = Number.parseInt(pidText.trim(), 10);
  if (!Number.isInteger(pid) || pid <= 0) return { kind: "stopped" };
  return alive(pid) ? { kind: "running", pid } : { kind: "stale", pid };
}

export function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    // EPERM means it exists but belongs to someone else — still alive.
    return (err as NodeJS.ErrnoException).code === "EPERM";
  }
}

// ─── whole state ────────────────────────────────────────────────────────

export interface State {
  worker: WorkerStatus;
  bench: BenchReport | null;
  benchPath: string;
  /** Set when bench.json exists but could not be parsed. */
  benchError: string | null;
  modules: string[];
  loadedAt: Date;
}

export const MODULES = ["transcode", "coinpay", "infernet"];

function readOrNull(path: string): string | null {
  try {
    return readFileSync(path, "utf8");
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") return null;
    throw err;
  }
}

export function loadState(env?: Env, now = new Date()): State {
  const bPath = benchPath(env);
  const raw = readOrNull(bPath);
  const bench = raw === null ? null : parseBenchReport(raw);
  return {
    worker: workerStatusFrom(readOrNull(workerPidPath(env)), pidAlive),
    bench,
    benchPath: bPath,
    benchError: raw !== null && bench === null ? "bench.json is not a valid report" : null,
    modules: MODULES,
    loadedAt: now,
  };
}
