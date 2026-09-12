#!/usr/bin/env bun
/**
 * c0mpute-tui — interactive terminal dashboard on @profullstack/hqtui.
 *
 * Launched by `c0mpute tui`. Reads the node's data dir (worker pid, the
 * `c0mpute bench` report) and redraws every couple of seconds; no daemon
 * connection yet. See dips/0008-ui-strategy.md.
 */
import { createApp, matchKey } from "@profullstack/hqtui";
import { loadState, type State } from "./state.ts";
import { c0mputeTheme } from "./theme.ts";
import { renderDashboard } from "./view.ts";

const REFRESH_MS = 2000;

async function main(): Promise<void> {
  const app = await createApp({
    theme: c0mputeTheme,
    title: "c0mpute · tui",
    mouse: false,
    fps: 20,
    quitKeys: ["q", "escape", "ctrl+c"],
  });

  let state: State = loadState();
  const reload = (): void => {
    state = loadState();
    app.invalidate();
  };

  const timer = setInterval(reload, REFRESH_MS);
  timer.unref?.();

  app.on("key", (event) => {
    if (matchKey(event, "r")) reload();
  });

  app.render(({ ui, theme }) => renderDashboard(ui, state, theme));

  try {
    await app.start();
  } finally {
    clearInterval(timer);
  }
}

main().catch((err) => {
  console.error(err instanceof Error ? err.message : String(err));
  process.exit(1);
});
