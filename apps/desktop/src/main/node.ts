// The local node, driven through the CLI: status, start/stop, service install,
// nickname, private mode, logs, config, hosting publishes and the
// `chat connect hosting` bridge.

import type { ChildProcess } from "node:child_process";
import { EventEmitter } from "node:events";
import { closeSync, existsSync, openSync, readFileSync, readSync, statSync, writeFileSync, mkdirSync, renameSync } from "node:fs";
import { dirname } from "node:path";
import type { Cli } from "./cli";
import { isLogLine, LineSplitter, parseProgressLine, parsePublishSummary, type PublishProgress } from "./progress";
import { applyNodeSettings, checkConfigText, readNodeSettings, type NodeSettings } from "./toml-edit";

/** `c0mpute status --json` (node/crates/c0mpute-cli/src/main.rs, worker_status_json). */
export interface NodeStatus {
  version: string;
  worker: { running: boolean; pid: number | null; stale_pid: boolean; pid_file: string; log_file: string };
  peer_id: string | null;
  publisher: string | null;
  name: string | null;
  private: boolean;
  auto_update: boolean;
  storage: boolean;
  hosting: boolean;
  roles: string[];
  gateway_bind: string;
  payments: { enabled: boolean; rail: string; api_key_env: string; api_key_set: boolean };
  bench: { score: number; quick: boolean; cores: number; at: string; version: string } | null;
  config_path: string;
  config_dir: string | null;
  data_dir: string | null;
}

/** The URL to reach a gateway bound to `bind` from this machine. */
export function gatewayUrl(bind: string): string {
  const m = /^(.*):(\d+)$/.exec(bind.trim());
  if (!m) return "http://127.0.0.1:7777";
  let host = m[1];
  if (host === "0.0.0.0" || host === "") host = "127.0.0.1";
  if (host === "[::]" || host === "::") host = "[::1]";
  return `http://${host}:${m[2]}`;
}

export function workerStartArgs(o: { roles?: string; noStorage?: boolean; noHosting?: boolean; private?: boolean }): string[] {
  const args = ["worker", "start", "-d"];
  const roles = (o.roles ?? "")
    .split(/[\s,]+/)
    .map((r) => r.trim())
    .filter((r) => /^[a-z0-9][a-z0-9-]*$/.test(r));
  if (roles.length) args.push("--roles", roles.join(","));
  if (o.noStorage) args.push("--no-storage");
  if (o.noHosting) args.push("--no-hosting");
  if (o.private) args.push("--private");
  return args;
}

/** Last `bytes` of a file as lines. */
export function tailFile(path: string, bytes = 64 * 1024): string[] {
  if (!existsSync(path)) return [];
  const size = statSync(path).size;
  const start = Math.max(0, size - bytes);
  const fd = openSync(path, "r");
  try {
    const buf = Buffer.alloc(size - start);
    readSync(fd, buf, 0, buf.length, start);
    const lines = buf.toString("utf8").split(/\r?\n/);
    if (start > 0) lines.shift(); // partial first line
    return lines.filter((l) => l.length);
  } finally {
    closeSync(fd);
  }
}

export class NodeManager {
  constructor(private cli: Cli) {}

  status(): Promise<NodeStatus> {
    return this.cli.json<NodeStatus>(["status", "--json"]);
  }

  async start(o: Parameters<typeof workerStartArgs>[0]) {
    const r = await this.cli.run(workerStartArgs(o), { timeoutMs: 120_000 });
    if (r.code !== 0) throw new Error((r.stderr || r.stdout).trim() || `worker start exited ${r.code}`);
    return (r.stdout + r.stderr).trim();
  }

  async stop() {
    const r = await this.cli.run(["worker", "stop"], { timeoutMs: 60_000 });
    if (r.code !== 0) throw new Error((r.stderr || r.stdout).trim());
    return (r.stdout + r.stderr).trim();
  }

  async installService() {
    if (process.platform !== "linux") throw new Error("running as a service is systemd-only (Linux) for now");
    const r = await this.cli.run(["worker", "install"], { timeoutMs: 120_000 });
    if (r.code !== 0) throw new Error((r.stderr || r.stdout).trim());
    return (r.stdout + r.stderr).trim();
  }

  async setName(handle: string) {
    const args = handle ? ["name", handle] : ["name", "--clear"];
    const r = await this.cli.run(args);
    if (r.code !== 0) throw new Error((r.stderr || r.stdout).trim());
    return r.stdout.trim();
  }

  async setPrivate(on: boolean) {
    const r = await this.cli.run(["private", on ? "on" : "off"]);
    if (r.code !== 0) throw new Error((r.stderr || r.stdout).trim());
    return r.stdout.trim();
  }

  async checkUpdate() {
    const r = await this.cli.run(["update", "--check"], { timeoutMs: 60_000 });
    return (r.stdout + r.stderr).trim();
  }

  async update() {
    const r = await this.cli.run(["update"], { timeoutMs: 300_000 });
    if (r.code !== 0) throw new Error((r.stderr || r.stdout).trim());
    return (r.stdout + r.stderr).trim();
  }

  logs(status: NodeStatus, lines = 300): string[] {
    return tailFile(status.worker.log_file).slice(-lines);
  }

  readConfig(status: NodeStatus): { path: string; text: string; settings: NodeSettings } {
    const path = status.config_path;
    const text = existsSync(path) ? readFileSync(path, "utf8") : "";
    const check = checkConfigText(text);
    return { path, text, settings: readNodeSettings(check.value ?? {}) };
  }

  private writeConfig(path: string, text: string) {
    mkdirSync(dirname(path), { recursive: true });
    const tmp = `${path}.desktop-tmp`;
    writeFileSync(tmp, text, { mode: 0o600 });
    renameSync(tmp, path);
  }

  saveSettings(status: NodeStatus, s: Partial<NodeSettings>) {
    const { path, text } = this.readConfig(status);
    this.writeConfig(path, applyNodeSettings(text, s));
  }

  saveRawConfig(status: NodeStatus, text: string) {
    const check = checkConfigText(text);
    if (!check.ok) throw new Error(check.error);
    this.writeConfig(status.config_path, text);
  }

  async gateway<T>(status: NodeStatus, path: string): Promise<T | null> {
    try {
      const r = await fetch(gatewayUrl(status.gateway_bind) + path, { signal: AbortSignal.timeout(4000) });
      if (!r.ok) return null;
      return (await r.json()) as T;
    } catch {
      return null;
    }
  }

  async mySites(status: NodeStatus) {
    return this.cli.json<{ target_copies: number; sites: Record<string, unknown>[] }>([
      "hosting",
      "ls",
      "--mine",
      "--json",
      "--gateway",
      gatewayUrl(status.gateway_bind),
    ]);
  }
}

// ── hosting publish / rm as tracked jobs ─────────────────────────────────

export interface HostingJob {
  id: string;
  kind: "publish" | "rm";
  name: string;
  dir?: string;
  disks?: string;
  state: "running" | "done" | "failed" | "cancelled";
  progress: PublishProgress;
  lines: string[];
  site?: string;
  url?: string;
  started: number;
  ended?: number;
}

export class HostingJobs extends EventEmitter {
  jobs = new Map<string, HostingJob>();
  private children = new Map<string, ChildProcess>();
  constructor(private cli: Cli) {
    super();
  }

  list(): HostingJob[] {
    return [...this.jobs.values()].sort((a, b) => b.started - a.started);
  }

  private track(job: HostingJob, args: string[]) {
    this.jobs.set(job.id, job);
    const child = this.cli.spawn(args);
    this.children.set(job.id, child);
    let stdout = "";
    const err = new LineSplitter();
    const out = new LineSplitter();
    const onLine = (l: string) => {
      job.lines.push(l);
      if (job.lines.length > 400) job.lines.splice(0, job.lines.length - 400);
      const p = parseProgressLine(l);
      if (p && p.phase !== "info") job.progress = { ...p, overall: p.overall ?? job.progress.overall };
      else if (p) job.progress = { ...job.progress, text: p.text };
      this.emit("update", job);
    };
    child.stderr?.on("data", (d: Buffer) => err.push(d.toString()).forEach(onLine));
    child.stdout?.on("data", (d: Buffer) => {
      stdout += d.toString();
      out.push(d.toString()).forEach(onLine);
    });
    child.on("error", (e) => onLine(`error: ${e.message}`));
    child.on("close", (code) => {
      [...err.flush(), ...out.flush()].forEach(onLine);
      this.children.delete(job.id);
      const sum = parsePublishSummary(stdout);
      job.site = sum.site ?? job.site;
      job.url = sum.url ?? job.url;
      job.ended = Date.now();
      if (job.state !== "cancelled") job.state = code === 0 ? "done" : "failed";
      job.progress = {
        ...job.progress,
        phase: job.state === "done" ? "done" : "failed",
        overall: job.state === "done" ? 1 : job.progress.overall,
        text: job.state === "done" ? (job.kind === "rm" ? "removed" : `published${sum.copies !== undefined ? ` (${sum.copies} copies)` : ""}`) : [...job.lines].reverse().find((l) => !isLogLine(l)) ?? `exited ${code}`,
      };
      this.emit("update", job);
    });
  }

  publish(dir: string, name: string, disks: string | null): HostingJob {
    if (!/^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(name)) throw new Error("site name: 1-63 of a-z, 0-9 and '-'");
    if (!existsSync(dir) || !statSync(dir).isDirectory()) throw new Error(`${dir} is not a folder`);
    const job: HostingJob = {
      id: `pub-${Date.now().toString(36)}`,
      kind: "publish",
      name,
      dir,
      disks: disks ?? undefined,
      state: "running",
      progress: { phase: "starting", overall: 0, text: "starting" },
      lines: [],
      started: Date.now(),
    };
    const args = ["hosting", "publish", dir, "--name", name];
    if (disks) args.push("--disks", disks);
    this.track(job, args);
    return job;
  }

  rm(name: string): HostingJob {
    if (!/^[a-z0-9-]{1,63}$/.test(name)) throw new Error("bad site name");
    const job: HostingJob = {
      id: `rm-${Date.now().toString(36)}`,
      kind: "rm",
      name,
      state: "running",
      progress: { phase: "starting", overall: 0, text: "taking the site down" },
      lines: [],
      started: Date.now(),
    };
    this.track(job, ["hosting", "rm", "--name", name]);
    return job;
  }

  cancel(id: string) {
    const c = this.children.get(id);
    const j = this.jobs.get(id);
    if (c && j) {
      j.state = "cancelled";
      c.kill("SIGINT");
    }
  }

  stopAll() {
    for (const c of this.children.values()) c.kill("SIGINT");
  }
}

// ── `c0mpute chat connect hosting --name <node>`, kept running ──────────

export class ConnectBridge extends EventEmitter {
  child: ChildProcess | null = null;
  lines: string[] = [];
  enabled = false;
  name = "";
  private restartTimer: NodeJS.Timeout | null = null;
  private failures = 0;
  constructor(private cli: Cli) {
    super();
  }

  get running() {
    return this.child !== null;
  }

  state() {
    return { enabled: this.enabled, name: this.name, running: this.running, lines: this.lines.slice(-200) };
  }

  private log(l: string) {
    this.lines.push(l);
    if (this.lines.length > 500) this.lines.splice(0, this.lines.length - 500);
    this.emit("update", this.state());
  }

  setEnabled(on: boolean, name: string) {
    if (on && name && !/^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(name)) throw new Error("node name: 1-63 of a-z, 0-9 and '-'");
    this.enabled = on;
    if (name !== this.name && this.child) this.child.kill("SIGINT");
    this.name = name;
    if (on) this.ensure();
    else this.stop();
    this.emit("update", this.state());
  }

  private ensure() {
    if (!this.enabled || this.child) return;
    const args = ["chat", "connect", "hosting"];
    if (this.name) args.push("--name", this.name);
    this.log(`$ c0mpute ${args.join(" ")}`);
    let child: ChildProcess;
    try {
      child = this.cli.spawn(args);
    } catch (e) {
      this.log(`error: ${e instanceof Error ? e.message : e}`);
      return;
    }
    this.child = child;
    const started = Date.now();
    const split = [new LineSplitter(), new LineSplitter()];
    child.stdout?.on("data", (d: Buffer) => split[0].push(d.toString()).forEach((l) => this.log(l)));
    child.stderr?.on("data", (d: Buffer) => split[1].push(d.toString()).forEach((l) => this.log(l)));
    child.on("error", (e) => this.log(`error: ${e.message}`));
    child.on("close", (code) => {
      this.child = null;
      this.log(`(exited ${code})`);
      this.failures = Date.now() - started > 60_000 ? 0 : this.failures + 1;
      if (this.enabled) {
        const delay = Math.min(300_000, 2000 * 2 ** Math.min(this.failures, 7));
        this.log(`restarting in ${Math.round(delay / 1000)} s`);
        this.restartTimer = setTimeout(() => this.ensure(), delay);
      }
      this.emit("update", this.state());
    });
    this.emit("update", this.state());
  }

  stop() {
    if (this.restartTimer) clearTimeout(this.restartTimer);
    this.restartTimer = null;
    this.child?.kill("SIGINT");
  }
}
