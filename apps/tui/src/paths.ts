/**
 * Where the Rust node keeps its state. Mirrors `config::data_dir()` in
 * c0mpute-core, which is `directories::ProjectDirs::from("com", "c0mpute",
 * "c0mpute").data_dir()`:
 *
 *   Linux    $XDG_DATA_HOME/c0mpute            (~/.local/share/c0mpute)
 *   macOS    ~/Library/Application Support/com.c0mpute.c0mpute
 *   Windows  %APPDATA%\c0mpute\c0mpute\data
 *
 * The TUI only ever reads from here.
 */
import { homedir } from "node:os";
import { join } from "node:path";

export interface Env {
  platform: NodeJS.Platform;
  home: string;
  xdgDataHome?: string;
  appData?: string;
}

export function currentEnv(): Env {
  return {
    platform: process.platform,
    home: homedir(),
    xdgDataHome: process.env.XDG_DATA_HOME,
    appData: process.env.APPDATA,
  };
}

export function dataDir(env: Env = currentEnv()): string {
  switch (env.platform) {
    case "darwin":
      return join(env.home, "Library", "Application Support", "com.c0mpute.c0mpute");
    case "win32":
      return join(env.appData ?? join(env.home, "AppData", "Roaming"), "c0mpute", "c0mpute", "data");
    default:
      return join(env.xdgDataHome && env.xdgDataHome !== "" ? env.xdgDataHome : join(env.home, ".local", "share"), "c0mpute");
  }
}

export const BENCH_FILE = "bench.json";
export const WORKER_PID_FILE = "worker.pid";

export function benchPath(env?: Env): string {
  return join(dataDir(env), BENCH_FILE);
}

export function workerPidPath(env?: Env): string {
  return join(dataDir(env), WORKER_PID_FILE);
}
