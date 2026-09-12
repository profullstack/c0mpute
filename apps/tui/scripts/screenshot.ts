#!/usr/bin/env bun
/**
 * Render the dashboard to plain text without a terminal, for docs and PRs.
 *
 *   bun apps/tui/scripts/screenshot.ts [width] [height]
 *
 * Reads the same data dir the live TUI reads (honours XDG_DATA_HOME).
 */
import { renderToText } from "@profullstack/hqtui";
import { loadState } from "../src/state.ts";
import { c0mputeTheme } from "../src/theme.ts";
import { renderDashboard } from "../src/view.ts";

const width = Number(process.argv[2] ?? 120);
const height = Number(process.argv[3] ?? 32);
const state = loadState();
process.stdout.write(
  renderToText(({ ui, theme }) => renderDashboard(ui, state, theme), { width, height, theme: c0mputeTheme }),
);
process.stdout.write("\n");
