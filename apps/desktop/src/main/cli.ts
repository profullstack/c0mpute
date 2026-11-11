// Finding and running the c0mpute CLI (the Rust binary,
// node/crates/c0mpute-cli). The app ships it in resources/cli/ (electron-builder
// extraResources), so one install gives both the window and the command.

import { spawn, type ChildProcess } from "node:child_process";
import { accessSync, constants, existsSync } from "node:fs";
import { homedir } from "node:os";
import { delimiter, join } from "node:path";

export const EXE = process.platform === "win32" ? "c0mpute.exe" : "c0mpute";

function executable(p: string): boolean {
  try {
    accessSync(p, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

export function onPath(name: string, pathVar = process.env.PATH ?? ""): string | null {
  for (const dir of pathVar.split(delimiter)) {
    if (!dir) continue;
    const p = join(dir, name);
    if (existsSync(p) && executable(p)) return p;
  }
  return null;
}

export interface CliLocation {
  path: string | null;
  source: "env" | "bundled" | "installed" | "path" | "missing";
}

/**
 * Which binary to run: $C0MPUTE_BIN, then the bundled one, then an install.sh
 * install (~/.c0mpute/bin), then PATH.
 */
export function locateCli(resourcesPath: string | undefined, env = process.env): CliLocation {
  if (env.C0MPUTE_BIN && existsSync(env.C0MPUTE_BIN)) return { path: env.C0MPUTE_BIN, source: "env" };
  if (resourcesPath) {
    const p = join(resourcesPath, "cli", EXE);
    if (existsSync(p)) return { path: p, source: "bundled" };
  }
  const installed = join(env.HOME || homedir(), ".c0mpute", "bin", EXE);
  if (existsSync(installed)) return { path: installed, source: "installed" };
  const p = onPath(EXE, env.PATH);
  return p ? { path: p, source: "path" } : { path: null, source: "missing" };
}

export interface RunResult {
  code: number | null;
  stdout: string;
  stderr: string;
}

export class Cli {
  constructor(
    public bin: string | null,
    private env: NodeJS.ProcessEnv = process.env,
  ) {}

  private need(): string {
    if (!this.bin) throw new Error("the c0mpute CLI was not found: reinstall the app, or install it with `curl -fsSL https://c0mpute.com/install.sh | sh`");
    return this.bin;
  }

  spawn(args: string[], opts: { env?: NodeJS.ProcessEnv; detached?: boolean; cwd?: string } = {}): ChildProcess {
    return spawn(this.need(), args, {
      env: { ...this.env, ...opts.env },
      stdio: ["ignore", "pipe", "pipe"],
      detached: opts.detached,
      cwd: opts.cwd,
      windowsHide: true,
    });
  }

  run(args: string[], opts: { timeoutMs?: number; env?: NodeJS.ProcessEnv } = {}): Promise<RunResult> {
    return new Promise((resolve) => {
      let child: ChildProcess;
      try {
        child = this.spawn(args, { env: opts.env });
      } catch (e) {
        resolve({ code: -1, stdout: "", stderr: e instanceof Error ? e.message : String(e) });
        return;
      }
      let stdout = "";
      let stderr = "";
      child.stdout?.on("data", (d) => (stdout += d));
      child.stderr?.on("data", (d) => (stderr += d));
      const timer = setTimeout(() => child.kill("SIGTERM"), opts.timeoutMs ?? 60_000);
      child.on("error", (e) => {
        clearTimeout(timer);
        resolve({ code: -1, stdout, stderr: stderr + e.message });
      });
      child.on("close", (code) => {
        clearTimeout(timer);
        resolve({ code, stdout, stderr });
      });
    });
  }

  async json<T>(args: string[], timeoutMs = 30_000): Promise<T> {
    const r = await this.run(args, { timeoutMs });
    if (r.code !== 0) throw new Error((r.stderr || r.stdout).trim() || `c0mpute ${args.join(" ")} exited ${r.code}`);
    return parseJsonOutput<T>(r.stdout);
  }
}

/** The JSON object in CLI stdout (tolerates log lines printed before it). */
export function parseJsonOutput<T>(stdout: string): T {
  const t = stdout.trim();
  try {
    return JSON.parse(t) as T;
  } catch {
    const i = t.indexOf("{");
    if (i === -1) throw new Error("the CLI printed no JSON (is it older than 0.3.29? update it)");
    return JSON.parse(t.slice(i)) as T;
  }
}

// ── the `c0mpute` command on PATH ───────────────────────────────────────

export function shimScript(target: string): string {
  return `#!/bin/sh\n# Written by the c0mpute desktop app: runs the CLI bundled with it.\nexec "${target.replace(/(["\\$`])/g, "\\$1")}" "$@"\n`;
}

export interface ShimPlan {
  action: "write" | "skip";
  shim: string;
  /** Copy the binary here first (an AppImage's mount path changes every run). */
  copyTo?: string;
  target: string;
  reason?: string;
}

/**
 * Where to put the `c0mpute` command. An existing c0mpute that is not our
 * shim (install.sh's ~/.c0mpute/bin one, say) is left alone.
 */
export function planShim(opts: {
  home: string;
  bundled: string;
  appImage?: string;
  existing: string | null;
  existingIsShim: boolean;
}): ShimPlan {
  const shim = join(opts.home, ".local", "bin", "c0mpute");
  if (opts.existing && !opts.existingIsShim) {
    return { action: "skip", shim, target: opts.existing, reason: `c0mpute is already installed at ${opts.existing}` };
  }
  if (opts.appImage) {
    const copyTo = join(opts.home, ".c0mpute", "desktop", "bin", "c0mpute");
    return { action: "write", shim, copyTo, target: copyTo };
  }
  return { action: "write", shim, target: opts.bundled };
}
