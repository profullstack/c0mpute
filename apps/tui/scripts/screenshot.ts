#!/usr/bin/env bun
/**
 * Render TUI tabs to plain text without a terminal, for docs and PRs.
 *
 *   bun apps/tui/scripts/screenshot.ts [--demo] [--tab chat|jobs|node|network|settings|all] [width] [height]
 *
 * --demo draws the fixed demo state (src/demo.ts). Without it, it draws this
 * machine: local files (sign-in, jobs, chat, bench), then waits for the same
 * probes the live app runs (c0mpute status --json, the log, c0mpute.com).
 */
import { renderToText } from "@profullstack/hqtui";
import { Api } from "../src/api.ts";
import { loadBench } from "../src/bench.ts";
import { loadCreds, loadSettings, paths, readJson } from "../src/config.ts";
import { Controller } from "../src/controller.ts";
import { demoState } from "../src/demo.ts";
import type { Job } from "../src/jobs.ts";
import { initialState, TABS, type AppState, type Convo, type Tab } from "../src/state.ts";
import { c0mputeTheme } from "../src/theme.ts";
import { renderApp } from "../src/views/app.ts";

const args = process.argv.slice(2);
const flag = (name: string) => {
  const i = args.indexOf(name);
  return i >= 0 ? args.splice(i, 2)[1] : undefined;
};
const demo = args.includes("--demo");
if (demo) args.splice(args.indexOf("--demo"), 1);
const tabArg = flag("--tab") ?? "all";
const width = Number(args[0] ?? 120);
const height = Number(args[1] ?? 32);
const tabs: Tab[] = tabArg === "all" ? [...TABS] : [tabArg as Tab];

async function liveState(): Promise<AppState> {
  const creds = loadCreds();
  const ctl = new Controller(
    initialState({
      creds,
      settings: loadSettings(),
      bench: loadBench(),
      jobs: readJson<Job[]>(paths.jobs()) ?? [],
      convo: readJson<Convo>(paths.chat()),
    }),
    { api: new Api(creds), invalidate: () => {}, persist: false, probes: false },
  );
  ctl.refreshWorker();
  await Promise.all([ctl.refreshStatus(), ctl.refreshLogs(), ctl.refreshVersion(), ctl.refreshNetwork(), ctl.refreshAccount(), ctl.refreshModels()]);
  ctl.runner.dispose();
  return { ...ctl.state, now: Date.now() };
}

const state = demo ? demoState() : await liveState();
for (const tab of tabs) {
  process.stdout.write(`── ${tab} (${width}x${height}) ${"─".repeat(Math.max(0, width - tab.length - 14))}\n`);
  process.stdout.write(renderToText(({ ui, theme }) => renderApp(ui, { ...state, tab }, theme), { width, height, theme: c0mputeTheme }));
  process.stdout.write("\n");
}
process.exit(0);
