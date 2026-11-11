/**
 * The local node, read the way the CLI reads it:
 *
 * - the worker: its PID file, else the `c0mpute worker start` process (a
 *   systemd unit writes no PID file), and whether a unit runs it;
 * - roles, gates and tags: `c0mpute status --json` (the worker's own checks,
 *   with the reason a role is not serving); older CLIs fall back to the text
 *   `c0mpute status`;
 * - jobs running now: the worker's child processes (sd-cli, ACE-Step,
 *   ffmpeg, blender, …) from /proc;
 * - recent jobs: `<data dir>/job-times/<workload>.ms` (average wall time,
 *   last finish = mtime);
 * - the log: `<data dir>/worker.log` for a daemon, else the journal;
 * - version and update: `c0mpute --version` and the release feed.
 *
 * Every probe is async with a timeout; the UI draws whatever has arrived.
 */
import { execFile } from "node:child_process";
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { dataDir, type Env } from "./paths.ts";

export interface Exec {
  code: number;
  stdout: string;
  stderr: string;
}

export function run(cmd: string, args: string[], timeoutMs = 10_000): Promise<Exec> {
  return new Promise((resolve) => {
    execFile(cmd, args, { timeout: timeoutMs, maxBuffer: 8 * 1024 * 1024, env: { ...process.env, C0MPUTE_NO_AUTO_UPDATE: "1", NO_COLOR: "1" } }, (err, stdout, stderr) => {
      const code = err ? (typeof (err as { code?: unknown }).code === "number" ? ((err as { code: number }).code) : 127) : 0;
      resolve({ code, stdout: String(stdout ?? ""), stderr: String(stderr ?? (err ? err.message : "")) });
    });
  });
}

/** The `c0mpute` binary: $C0MPUTE_BIN, PATH, then ~/.c0mpute/bin. */
export function c0mputeBin(env: NodeJS.ProcessEnv = process.env, home = homedir()): string {
  if (env.C0MPUTE_BIN) return env.C0MPUTE_BIN;
  for (const dir of (env.PATH ?? "").split(":")) {
    if (dir && existsSync(join(dir, "c0mpute"))) return join(dir, "c0mpute");
  }
  const local = join(home, ".c0mpute", "bin", "c0mpute");
  return existsSync(local) ? local : "c0mpute";
}

// ─── versions ───────────────────────────────────────────────────────────

export function parseVersion(text: string): string | null {
  return /(\d+\.\d+\.\d+)/.exec(text)?.[1] ?? null;
}

/** -1, 0, 1 like a comparator; unknown parts compare as 0. */
export function compareVersions(a: string, b: string): number {
  const pa = a.split(".").map((n) => Number.parseInt(n, 10) || 0);
  const pb = b.split(".").map((n) => Number.parseInt(n, 10) || 0);
  for (let i = 0; i < 3; i++) {
    if ((pa[i] ?? 0) !== (pb[i] ?? 0)) return (pa[i] ?? 0) < (pb[i] ?? 0) ? -1 : 1;
  }
  return 0;
}

// ─── the worker process ─────────────────────────────────────────────────

export interface WorkerFlags {
  roles: string[] | null;
  noStorage: boolean;
  noHosting: boolean;
  gpu: boolean;
  private: boolean;
}

/** Same rule as the Rust `node_status::worker_flags`: `… worker start [flags]`. */
export function workerFlags(argv: string[]): WorkerFlags | null {
  const at = argv.findIndex((a, i) => a === "worker" && argv[i + 1] === "start");
  if (at < 0) return null;
  const f: WorkerFlags = { roles: null, noStorage: false, noHosting: false, gpu: false, private: false };
  const rest = argv.slice(at + 2);
  const split = (v: string) => v.split(",").map((s) => s.trim()).filter(Boolean);
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i]!;
    if (a === "--roles") f.roles = split(rest[++i] ?? "");
    else if (a.startsWith("--roles=")) f.roles = split(a.slice(8));
    else if (a === "--no-storage") f.noStorage = true;
    else if (a === "--no-hosting") f.noHosting = true;
    else if (a === "--gpu") f.gpu = true;
    else if (a === "--private") f.private = true;
  }
  return f;
}

export interface ProcInfo {
  pid: number;
  ppid: number;
  argv: string[];
  /** Seconds since it started. */
  ageS: number;
  uid: number | null;
}

/** `/proc/<pid>/stat` → ppid and start time (clock ticks since boot). */
export function parseStat(stat: string): { ppid: number; startTicks: number } | null {
  const close = stat.lastIndexOf(")");
  if (close < 0) return null;
  const f = stat.slice(close + 2).split(" ");
  // fields after "(comm)": state=0, ppid=1, ... starttime is field 22 overall = index 19 here
  const ppid = Number(f[1]);
  const startTicks = Number(f[19]);
  return Number.isFinite(ppid) && Number.isFinite(startTicks) ? { ppid, startTicks } : null;
}

const CLK_TCK = 100;

export function readProcs(root = "/proc"): ProcInfo[] {
  let uptime = 0;
  try {
    uptime = Number.parseFloat(readFileSync(join(root, "uptime"), "utf8").split(" ")[0]!);
  } catch {
    return [];
  }
  const out: ProcInfo[] = [];
  let entries: string[];
  try {
    entries = readdirSync(root);
  } catch {
    return [];
  }
  for (const name of entries) {
    if (!/^\d+$/.test(name)) continue;
    try {
      const st = parseStat(readFileSync(join(root, name, "stat"), "utf8"));
      if (!st) continue;
      const argv = readFileSync(join(root, name, "cmdline"), "utf8").split("\0").filter(Boolean);
      let uid: number | null = null;
      try {
        uid = Number(/^Uid:\s+(\d+)/m.exec(readFileSync(join(root, name, "status"), "utf8"))?.[1] ?? Number.NaN);
        if (!Number.isFinite(uid)) uid = null;
      } catch {}
      out.push({ pid: Number(name), ppid: st.ppid, argv, ageS: Math.max(0, uptime - st.startTicks / CLK_TCK), uid });
    } catch {
      // gone, or hidden from us
    }
  }
  return out;
}

export function isWorker(p: ProcInfo): boolean {
  // An in-place update re-execs from /proc/self/exe: "…/c0mpute (deleted)".
  return (p.argv[0] ?? "").replace(/ \(deleted\)$/, "").endsWith("c0mpute") && workerFlags(p.argv) !== null;
}

/** Everything under `pid`, depth first. */
export function descendants(procs: ProcInfo[], pid: number): ProcInfo[] {
  const kids = new Map<number, ProcInfo[]>();
  for (const p of procs) kids.set(p.ppid, [...(kids.get(p.ppid) ?? []), p]);
  const out: ProcInfo[] = [];
  const walk = (id: number, depth: number) => {
    if (depth > 8) return;
    for (const k of kids.get(id) ?? []) {
      out.push(k);
      walk(k.pid, depth + 1);
    }
  };
  walk(pid, 0);
  return out;
}

/** What a worker child is doing, or null for helpers not worth a row. */
export function classifyProcess(argv: string[]): string | null {
  const exe = (argv[0] ?? "").split("/").pop() ?? "";
  const line = argv.join(" ");
  if (/(^|\/)sd(-cli)?$/.test(argv[0] ?? "") || exe === "sd-cli") return /wan|vid/i.test(line) ? "video (CPU, Wan 2.1)" : "image (Z-Image-Turbo)";
  if (/acestep|ace[-_]step|music[_-]worker/i.test(line)) return "music (ACE-Step)";
  if (exe === "ffmpeg") return "ffmpeg (transcode / mux)";
  if (exe === "blender") return "render (Blender)";
  if (exe === "tesseract") return "ocr (Tesseract)";
  if (/kokoro|piper|pocket[_-]tts|tts[_-]worker/i.test(line)) return "tts";
  if (/whisper|stt[_-]worker/i.test(line)) return "stt";
  if (/ddddocr|captcha/i.test(line)) return "captcha";
  if (/fastembed|embed[_-]worker/i.test(line)) return "embed";
  if (/peft|train/i.test(line) && /python/i.test(exe)) return "text fine-tune";
  if (exe === "infernet" || /\binfernet\b/.test(exe)) return null;
  if (exe === "docker" || exe === "podman") return `container (${argv.slice(1, 3).join(" ")})`;
  if (/python/.test(exe)) return `python ${argv.slice(1).find((a) => !a.startsWith("-"))?.split("/").pop() ?? ""}`.trim();
  return null;
}

// ─── logs ───────────────────────────────────────────────────────────────

export interface LogLine {
  time?: string;
  level: string;
  message: string;
}

const ANSI = /\x1b\[[0-9;]*[A-Za-z]/g;

/** A tracing line: `2026-11-11T00:57:06.808Z  INFO target: message k=v`. */
export function parseLogLine(raw: string): LogLine | null {
  const line = raw.replace(ANSI, "").trim();
  if (!line) return null;
  const m = /^(\d{4}-\d\d-\d\dT(\d\d:\d\d:\d\d))\S*\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s+(.*)$/.exec(line);
  if (!m) return { level: "INFO", message: line };
  return { time: m[2], level: m[3]!, message: m[4]! };
}

/** The last `n` lines worth showing (INFO and up). */
export function tailLog(text: string, n: number): LogLine[] {
  const out: LogLine[] = [];
  const lines = text.split("\n");
  for (let i = lines.length - 1; i >= 0 && out.length < n; i--) {
    const l = parseLogLine(lines[i]!);
    if (l && l.level !== "DEBUG" && l.level !== "TRACE") out.push(l);
  }
  return out.reverse();
}

// ─── job history ────────────────────────────────────────────────────────

export interface JobTime {
  workload: string;
  avgMs: number;
  lastAt: number;
}

export function readJobTimes(dir: string): JobTime[] {
  try {
    return readdirSync(dir)
      .filter((f) => f.endsWith(".ms"))
      .map((f) => {
        const p = join(dir, f);
        const ms = Number.parseInt(readFileSync(p, "utf8").trim(), 10);
        return { workload: f.slice(0, -3), avgMs: Number.isFinite(ms) ? ms : 0, lastAt: statSync(p).mtimeMs };
      })
      .sort((a, b) => b.lastAt - a.lastAt);
  } catch {
    return [];
  }
}

// ─── c0mpute status --json ──────────────────────────────────────────────

export interface RoleStatus {
  role: string;
  serving: boolean;
  reason: string | null;
  detail: string | null;
}

/**
 * `c0mpute status --json`: the base report the desktop app also reads
 * (version, worker, peer id, nickname, config) plus, from the CLI that
 * ships with this TUI, `role_states` (serving or gated and why), `tags`,
 * `host` and the worker process it found.
 */
export interface NodeStatusJson {
  version: string;
  config_path: string;
  peer_id: string | null;
  name: string | null;
  private: boolean;
  auto_update: boolean;
  /** Configured roles (the base report); role_states has the effective set. */
  roles?: string[];
  worker_process: { pid: number; argv: string[] } | null;
  roles_source: string;
  hardware: string;
  role_states: RoleStatus[];
  tags: string[];
  host: { cpus: number; job_threads: number; load_1m: number; ram_total_bytes: number; ram_free_bytes: number };
}

export function parseStatusJson(text: string): NodeStatusJson | null {
  try {
    const v = JSON.parse(text) as NodeStatusJson;
    return v && Array.isArray(v.role_states) && Array.isArray(v.tags) ? v : null;
  } catch {
    return null;
  }
}

// ─── the whole snapshot ─────────────────────────────────────────────────

export interface WorkerInfo {
  state: "running" | "stopped" | "stale";
  pid: number | null;
  via: "pid file" | "systemd" | "process" | null;
  /** The worker runs as someone else (a root system unit), so its config may differ. */
  otherUser: boolean;
  ageS: number | null;
  flags: WorkerFlags | null;
}

export interface RunningJob {
  pid: number;
  what: string;
  ageS: number;
}

export interface NodeSnapshot {
  cliPath: string;
  cliVersion: string | null;
  latest: string | null;
  worker: WorkerInfo;
  status: NodeStatusJson | null;
  /** Old CLI without --json: the text status lines. */
  statusText: string[] | null;
  statusError: string | null;
  statusAt: number | null;
  running: RunningJob[];
  jobTimes: JobTime[];
  logs: LogLine[];
  logSource: string;
}

export function emptyNode(cliPath = "c0mpute"): NodeSnapshot {
  return {
    cliPath,
    cliVersion: null,
    latest: null,
    worker: { state: "stopped", pid: null, via: null, otherUser: false, ageS: null, flags: null },
    status: null,
    statusText: null,
    statusError: null,
    statusAt: null,
    running: [],
    jobTimes: [],
    logs: [],
    logSource: "",
  };
}

function readPidFile(path: string): number | null {
  try {
    const n = Number.parseInt(readFileSync(path, "utf8").trim(), 10);
    return Number.isInteger(n) && n > 0 ? n : null;
  } catch {
    return null;
  }
}

/** Worker + running jobs from /proc and the PID file. Cheap; runs every few seconds. */
/** The systemd unit a process runs in, from /proc/<pid>/cgroup. */
export function unitOf(pid: number): string | null {
  try {
    return /\/([^/]+\.service)/.exec(readFileSync(`/proc/${pid}/cgroup`, "utf8"))?.[1] ?? null;
  } catch {
    return null;
  }
}

export function probeWorker(
  env?: Env,
  procs: ProcInfo[] = readProcs(),
  myUid = process.getuid?.() ?? -1,
  unit: (pid: number) => string | null = unitOf,
): { worker: WorkerInfo; running: RunningJob[] } {
  const dir = dataDir(env);
  const pidFile = readPidFile(join(dir, "worker.pid"));
  const byPid = new Map(procs.map((p) => [p.pid, p]));
  let proc = pidFile !== null ? byPid.get(pidFile) : undefined;
  let via: WorkerInfo["via"] = proc && isWorker(proc) ? "pid file" : null;
  if (!via) {
    // Several workers on one box (other users, a unit taking over): the
    // c0mpute-worker unit is the node, then one of ours, then the oldest.
    const rank = (p: ProcInfo) => (/c0mpute-worker/.test(unit(p.pid) ?? "") ? 0 : 2) + (p.uid === myUid ? 0 : 1);
    proc = procs.filter(isWorker).sort((a, b) => rank(a) - rank(b) || a.pid - b.pid)[0];
    via = proc ? "process" : null;
  }
  if (proc && via !== "pid file" && /c0mpute/.test(unit(proc.pid) ?? "")) {
    via = "systemd";
  }
  if (!proc) {
    return {
      worker: { state: pidFile !== null ? "stale" : "stopped", pid: pidFile, via: null, otherUser: false, ageS: null, flags: null },
      running: [],
    };
  }
  const running = descendants(procs, proc.pid)
    .map((p) => ({ pid: p.pid, what: classifyProcess(p.argv), ageS: p.ageS }))
    .filter((r): r is RunningJob => r.what !== null);
  return {
    worker: {
      state: "running",
      pid: proc.pid,
      via,
      otherUser: proc.uid !== null && myUid >= 0 && proc.uid !== myUid,
      ageS: proc.ageS,
      flags: workerFlags(proc.argv),
    },
    running,
  };
}

/** The worker's last log lines: the daemon log if fresh, else the journal. */
export async function probeLogs(worker: WorkerInfo, n = 40, env?: Env): Promise<{ lines: LogLine[]; source: string }> {
  const file = join(dataDir(env), "worker.log");
  try {
    const st = statSync(file);
    if (worker.via === "pid file" || Date.now() - st.mtimeMs < 10 * 60_000) {
      const size = st.size;
      const fd = await import("node:fs/promises").then((fs) => fs.open(file, "r"));
      const len = Math.min(size, 256 * 1024);
      const buf = Buffer.alloc(len);
      await fd.read(buf, 0, len, size - len);
      await fd.close();
      return { lines: tailLog(buf.toString("utf8"), n), source: file };
    }
  } catch {}
  const attempts: [string, string[]][] = [
    ["journalctl", ["-u", "c0mpute-worker", "-n", "600", "-o", "cat", "--no-pager"]],
    ["journalctl", ["--user", "-u", "c0mpute-worker", "-n", "600", "-o", "cat", "--no-pager"]],
  ];
  for (const [cmd, args] of attempts) {
    const r = await run(cmd, args, 8000);
    const lines = tailLog(r.stdout, n);
    if (r.code === 0 && lines.length && !/^-- No entries --$/.test(lines[0]?.message ?? "")) {
      return { lines, source: `${cmd} ${args.slice(0, args.indexOf("-n")).join(" ")}` };
    }
  }
  return {
    lines: [],
    source: worker.state === "running" ? "no log found (worker.log empty and the journal is not readable)" : "worker not running",
  };
}

export async function probeStatus(bin: string): Promise<{ status: NodeStatusJson | null; text: string[] | null; error: string | null }> {
  const r = await run(bin, ["status", "--json"], 45_000);
  const status = r.code === 0 ? parseStatusJson(r.stdout) : null;
  if (status) return { status, text: null, error: null };
  if (r.code === 127) return { status: null, text: null, error: `c0mpute not found (${bin}); install: curl -fsSL https://c0mpute.com/install.sh | sh` };
  // An older CLI: no --json. Show what the text status says.
  const t = await run(bin, ["status"], 30_000);
  if (t.code === 0) {
    const lines = t.stdout.split("\n").filter((l) => l.trim() && !/^[{}\]\["]|^\s+"/.test(l));
    return { status: null, text: lines.slice(0, 30), error: "this c0mpute is older than the TUI: `c0mpute update` adds per-role gates" };
  }
  return { status: null, text: null, error: (r.stderr || t.stderr || "c0mpute status failed").trim().split("\n").slice(-1)[0] ?? null };
}

export async function probeVersion(bin: string): Promise<string | null> {
  const r = await run(bin, ["--version"], 8000);
  return r.code === 0 ? parseVersion(r.stdout) : null;
}

export async function probeLatest(url = "https://c0mpute.com/releases/latest.json"): Promise<string | null> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(10_000) });
    const j = (await res.json()) as { version?: string };
    return typeof j.version === "string" ? j.version : null;
  } catch {
    return null;
  }
}

export function jobTimesDir(env?: Env): string {
  return join(dataDir(env), "job-times");
}
