#!/usr/bin/env bun
/**
 * c0mpute-tui: the c0mpute terminal app, launched by `c0mpute tui`.
 *
 *   1 Chat      c0mpute.com/chat in the terminal: streaming answers, model
 *               picker, /image /video /music /album /promo /push /connect
 *   2 Jobs      every generation job: progress, stage, worker, ETA, files
 *   3 Node      this machine's worker: roles serving or gated and why, tags,
 *               running jobs, log, version, bench
 *   4 Network   workers online, per-role counts, node board, prices, balance
 *   5 Settings  sign-in, default model, downloads dir, 18+ mode, previews
 *
 * The first frame draws from local files only; everything on the network or
 * in a subprocess arrives after. See dips/0008-ui-strategy.md.
 */
import { behindMosh, createApp, imageSupport, insertText, kittyDeleteImage } from "@profullstack/hqtui";
import pkg from "../package.json" with { type: "json" };
import { Api } from "./api.ts";
import { loadBench } from "./bench.ts";
import { loadCreds, loadSettings, paths, readJson } from "./config.ts";
import { Controller } from "./controller.ts";
import type { Job } from "./jobs.ts";
import { handleKey } from "./keys.ts";
import { initialState, type Convo } from "./state.ts";
import { c0mputeTheme } from "./theme.ts";
import { renderApp } from "./views/app.ts";

// package.json is installed next to src/ (see install_tui in main.rs).
const VERSION: string = pkg.version;

function usage(): void {
  process.stdout.write(`c0mpute tui ${VERSION}: chat, jobs, your node and the network, in the terminal

  c0mpute tui              start (Tab / 1-5 switch tabs, ? for keys)
  c0mpute tui --version

Sign in once with \`c0mpute chat login\` (or L in Settings).
Env: C0MPUTE_API_KEY, C0MPUTE_URL, C0MPUTE_BIN, HQTUI_IMAGES=1 (kitty images under tmux/SSH).
`);
}

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  if (args.includes("-h") || args.includes("--help")) return usage();
  if (args.includes("-V") || args.includes("--version")) return void process.stdout.write(`c0mpute-tui ${VERSION}\n`);

  const creds = loadCreds();
  const settings = loadSettings();
  const jobs = (readJson<Job[]>(paths.jobs()) ?? []).filter((j) => j && typeof j.id === "string" && j.kind);
  const convo = readJson<Convo>(paths.chat());
  const imagesInTerminal = imageSupport() === "kitty" && !behindMosh();

  const app = await createApp({
    theme: c0mputeTheme,
    title: "c0mpute",
    fps: 20,
    // q is a letter in the chat input; keys.ts decides when it quits.
    quitKeys: [],
    focusNavigation: false,
  });

  const api = new Api(creds);
  const ctl = new Controller(
    initialState({
      creds,
      settings,
      bench: loadBench(),
      jobs,
      convo: convo && Array.isArray(convo.messages) ? convo : null,
      imagesInTerminal,
    }),
    {
      api,
      invalidate: () => app.invalidate(),
      write: (seq) => app.terminal.write(seq),
    },
  );

  let quitting = false;
  const quit = () => {
    if (quitting) return;
    quitting = true;
    ctl.stop();
    if (ctl.state.preview?.imageId) app.terminal.write(kittyDeleteImage(ctl.state.preview.imageId));
    app.quit();
  };

  app.on("key", (e) => {
    if (handleKey(ctl, e) === "quit") quit();
  });
  app.on("paste", (e) => {
    const s = ctl.state;
    if (s.editing) ctl.setEditing(insertText(s.editing, e.text));
    else if (s.tab === "chat") {
      ctl.setTyping(true);
      ctl.setInput(insertText(ctl.state.chat.input, e.text));
    }
  });
  app.on("resize", ({ height }) => {
    ctl.state = { ...ctl.state, rows: height };
  });

  app.render(({ ui, theme, height }) => {
    if (ctl.state.rows !== height) ctl.state = { ...ctl.state, rows: height };
    renderApp(ui, ctl.state, theme, {
      onTab: (t) => ctl.setTab(t),
      onKey: (k) => {
        if (k === "q") return quit();
        const key = k === "↑↓" ? "down" : k.toLowerCase();
        if (handleKey(ctl, { type: "key", name: key, key, char: key.length === 1 ? key : undefined, ctrl: false, alt: false, shift: false, raw: key }) === "quit") quit();
      },
      onPick: (i) => ctl.choosePicker(i),
      onCloseOverlay: () => ctl.closeOverlays(),
      chat: { onScroll: (d) => ctl.scrollChat(d), onFocusInput: () => ctl.setTyping(true) },
      jobs: { onSelect: (i) => ctl.moveJob(i - ctl.state.jobSel), onScroll: (d) => ctl.moveJob(d) },
      node: { onScrollRoles: (d) => ctl.scrollNode(d) },
      network: { onScroll: (d) => ctl.scrollNet(d) },
      settings: {
        onSelect: (i) => {
          ctl.moveSetting(i - ctl.state.settingsSel);
        },
      },
    });
  });

  ctl.start();
  process.on("SIGTERM", quit);
  try {
    await app.start();
  } finally {
    ctl.stop();
  }
}

main().catch((err) => {
  console.error(err instanceof Error ? err.message : String(err));
  process.exit(1);
});
