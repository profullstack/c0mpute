// c0mpute desktop: main process.
//
// The window's renderer is a local management UI (Node, Hosting, Tasks, Agent,
// MCP servers, Skills). The Chat tab is c0mpute.com/chat itself in a
// WebContentsView with a persistent session, so it is the same code as the
// web. Everything that touches the machine goes through this process: the
// bundled CLI, ~/mcps, ~/skills and config.toml.

import { app, BrowserWindow, dialog, ipcMain, Menu, nativeImage, Notification, session, shell, Tray, WebContentsView } from "electron";
import { chmodSync, copyFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { basename, join } from "node:path";
import { readCreds, api, complete, models, chatJsonPath, type Me } from "./account";
import { AgentSession, type AgentEvent, type Decision } from "./agent";
import { buildTools } from "./agent-tools";
import { Cli, cliOutput, locateCli, onPath, planShim, shimScript } from "./cli";
import { matchMediaPoll, pollJob, type MediaJob } from "./jobs";
import { McpManager } from "./mcp-client";
import {
  claudeConfigPaths,
  defaultMcpDir,
  deleteServer,
  importServers,
  loadMcpDir,
  maskServer,
  placeholders,
  readClaudeServers,
  saveServer,
  type McpServer,
} from "./mcp-config";
import { ConnectBridge, HostingJobs, NodeManager, type NodeStatus } from "./node";
import { lookupVar, secretNames, setSecret } from "./secrets";
import { loadSettings, saveSettings, type Settings } from "./settings";
import { createSkill, defaultSkillsDir, importSkillFolder, importSkillZip, loadSkills, skillsPrompt } from "./skills";
import { checkForUpdate } from "./updater";
import { LineSplitter } from "./progress";

const VERSION = app.getVersion();
const CHAT_PARTITION = "persist:c0mpute-chat";
const isDev = !app.isPackaged;

// Electron's default userData would be ~/.config/c0mpute, which is the CLI's
// own config dir (config.toml, identity.key): keep Chromium's files out of it.
app.setPath("userData", join(app.getPath("appData"), "c0mpute-desktop"));

if (!app.requestSingleInstanceLock()) {
  app.quit();
}

let settings: Settings = loadSettings();
const persist = () => saveSettings(settings);

const cliLoc = locateCli(app.isPackaged ? process.resourcesPath : join(__dirname, "..", "..", "resources"));
const cli = new Cli(cliLoc.path);
const nodeMgr = new NodeManager(cli);
const hosting = new HostingJobs(cli);
const bridge = new ConnectBridge(cli);
const mcp = new McpManager(lookupVar);
let mcpServers: McpServer[] = [];
let mcpErrors: { file: string; error: string }[] = [];

let win: BrowserWindow | null = null;
let chatView: WebContentsView | null = null;
let tray: Tray | null = null;
let quitting = false;
let lastStatus: NodeStatus | null = null;
let lastMe: Me | null = null;

const iconPath = () => join(__dirname, "..", "renderer", "icon.png");

function send(channel: string, payload: unknown) {
  if (win && !win.isDestroyed()) win.webContents.send(channel, payload);
}

// ── chat view (c0mpute.com/chat) ───────────────────────────────────────

const IN_APP_HOSTS = [/(^|\.)c0mpute\.com$/, /(^|\.)coinpayportal\.com$/, /^localhost$/, /^127\.0\.0\.1$/];
function inApp(url: string): boolean {
  try {
    const u = new URL(url);
    return (u.protocol === "https:" || u.protocol === "http:") && IN_APP_HOSTS.some((r) => r.test(u.hostname));
  } catch {
    return false;
  }
}

function chatUrl() {
  return `${process.env.C0MPUTE_CHAT_URL || readCreds().url}/chat`;
}

function popup(url: string) {
  const child = new BrowserWindow({
    parent: win ?? undefined,
    width: 520,
    height: 760,
    title: "Sign in",
    webPreferences: { partition: CHAT_PARTITION, sandbox: true, contextIsolation: true },
  });
  child.removeMenu();
  child.webContents.setWindowOpenHandler(({ url: u }) => {
    if (inApp(u)) child.loadURL(u);
    else shell.openExternal(u);
    return { action: "deny" };
  });
  child.loadURL(url);
  return child;
}

function ensureChatView() {
  if (chatView || !win) return chatView;
  chatView = new WebContentsView({ webPreferences: { partition: CHAT_PARTITION, sandbox: true, contextIsolation: true } });
  const wc = chatView.webContents;
  // OAuth (CoinPay) and c0mpute links stay inside the app; anything else opens
  // in the browser.
  wc.setWindowOpenHandler(({ url }) => {
    if (inApp(url)) popup(url);
    else shell.openExternal(url);
    return { action: "deny" };
  });
  wc.on("will-navigate", (e, url) => {
    if (!inApp(url)) {
      e.preventDefault();
      shell.openExternal(url);
    }
  });
  win.contentView.addChildView(chatView);
  chatView.setVisible(false);
  wc.loadURL(chatUrl());
  return chatView;
}

function watchChatSession() {
  const ses = session.fromPartition(CHAT_PARTITION);
  ses.webRequest.onCompleted({ urls: ["https://*/api/v1/*", "http://*/api/v1/*"] }, (d) => {
    if (d.method !== "GET") return;
    const m = matchMediaPoll(d.url);
    if (m && !settings.mediaJobs.some((j) => j.id === m.id)) {
      settings.mediaJobs.unshift({ ...m, at: Date.now() });
      settings.mediaJobs = settings.mediaJobs.slice(0, 100);
      persist();
      mediaState.set(m.id, { ...m, at: Date.now(), status: "running" });
      send("tasks:update", null);
    }
  });
}

// ── media job polling ───────────────────────────────────────────────────

const mediaState = new Map<string, MediaJob>();
async function pollMedia() {
  const day = Date.now() - 24 * 3600_000;
  for (const j of settings.mediaJobs) {
    if (j.at < day) continue;
    const cur = mediaState.get(j.id) ?? { ...(j as { kind: MediaJob["kind"]; id: string; at: number }), status: "running" as const };
    if (cur.status === "done" || cur.status === "failed") continue;
    mediaState.set(j.id, await pollJob(cur));
  }
  send("tasks:update", null);
}

// ── tray ───────────────────────────────────────────────────────────────

async function refreshStatus() {
  try {
    lastStatus = await nodeMgr.status();
  } catch {
    lastStatus = null;
  }
  try {
    lastMe = readCreds().key ? await api<Me>("/api/v1/me") : null;
  } catch {
    /* keep the last one */
  }
  updateTray();
}

function updateTray() {
  if (!tray) return;
  const running = lastStatus?.worker.running ?? false;
  const earned = lastMe?.balance?.earned_spendable_usd;
  tray.setToolTip(`c0mpute: node ${running ? "running" : "stopped"}${earned ? `, earned $${Number(earned).toFixed(4)}` : ""}`);
  tray.setContextMenu(
    Menu.buildFromTemplate([
      { label: `Node: ${lastStatus ? (running ? `running (pid ${lastStatus.worker.pid})` : "stopped") : "CLI unavailable"}`, enabled: false },
      { label: `Earned: ${earned ? `$${Number(earned).toFixed(4)}` : "sign in to see"}`, enabled: false },
      { type: "separator" },
      {
        label: running ? "Stop node" : "Start node",
        enabled: !!lastStatus,
        click: async () => {
          try {
            if (running) await nodeMgr.stop();
            else await nodeMgr.start(settings.worker);
          } catch (e) {
            new Notification({ title: "c0mpute", body: e instanceof Error ? e.message : String(e) }).show();
          }
          await refreshStatus();
        },
      },
      { label: "Open c0mpute", click: () => showWindow() },
      { type: "separator" },
      {
        label: "Quit",
        click: () => {
          quitting = true;
          app.quit();
        },
      },
    ]),
  );
}

function createTray() {
  try {
    const img = nativeImage.createFromPath(iconPath()).resize({ width: 22, height: 22 });
    tray = new Tray(img);
    tray.on("click", () => showWindow());
    updateTray();
  } catch {
    tray = null; // no system tray (some Linux desktops): closing the window quits
  }
}

function showWindow() {
  if (!win) createWindow();
  win?.show();
  win?.focus();
}

function createWindow() {
  win = new BrowserWindow({
    width: 1280,
    height: 840,
    minWidth: 900,
    minHeight: 600,
    title: "c0mpute",
    icon: iconPath(),
    backgroundColor: "#0b0d10",
    webPreferences: {
      preload: join(__dirname, "..", "preload", "index.cjs"),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
    },
  });
  win.removeMenu();
  win.webContents.setWindowOpenHandler(({ url }) => {
    shell.openExternal(url);
    return { action: "deny" };
  });
  win.webContents.on("will-navigate", (e) => e.preventDefault());
  win.loadFile(join(__dirname, "..", "renderer", "index.html"));
  win.on("close", (e) => {
    if (!quitting && tray) {
      e.preventDefault();
      win?.hide();
    }
  });
  win.on("closed", () => {
    win = null;
    chatView = null;
  });
  ensureChatView();
}

// ── agent ──────────────────────────────────────────────────────────────

const pendingConfirms = new Map<string, (d: Decision) => void>();
let agentAbort: AbortController | null = null;

function currentSkills() {
  return loadSkills(defaultSkillsDir(), settings.skillsDisabled);
}

const agent = new AgentSession(
  {
    complete: (messages, model, signal) => complete(messages, model, signal),
    tools: async () => buildTools({ mcp, skills: currentSkills(), settings }),
    systemExtra: () => skillsPrompt(currentSkills()),
    confirm: ({ call, tool }) =>
      new Promise<Decision>((resolve) => {
        pendingConfirms.set(call.id, resolve);
        send("agent:confirm", { id: call.id, name: call.name, source: tool.source, arguments: call.arguments, canAlways: tool.source !== "shell" });
        if (win && !win.isFocused()) win.flashFrame(true);
      }),
    emit: (ev: AgentEvent) => send("agent:event", ev),
  },
  { model: settings.agentModel || undefined },
);

// ── MCP ────────────────────────────────────────────────────────────────

async function reloadMcp() {
  const r = loadMcpDir(defaultMcpDir());
  mcpServers = r.servers;
  mcpErrors = r.errors;
  await mcp.sync(mcpServers);
  send("mcp:update", null);
}

function mcpView() {
  const names = new Set(secretNames());
  return {
    dir: defaultMcpDir(),
    errors: mcpErrors,
    servers: mcpServers.map((s) => {
      const c = mcp.conns.get(s.name);
      return {
        ...maskServer(s),
        state: c?.state ?? "off",
        error: c?.error,
        tools: c?.tools.map((t) => ({ name: t.name, description: t.description })) ?? [],
        stderr: c?.stderr.slice(-20) ?? [],
        allowed: settings.mcpAllowed.includes(s.name),
        vars: placeholders(s).map((v) => ({ name: v, set: process.env[v] ? "env" : names.has(v) ? "app" : null })),
      };
    }),
  };
}

// ── sign in (`c0mpute chat login`) ─────────────────────────────────────

function login() {
  const child = cli.spawn(["chat", "login"]);
  const split = [new LineSplitter(), new LineSplitter()];
  let opened = false;
  const onLine = (l: string) => {
    send("account:login", { line: l });
    const m = /(https?:\/\/\S+)/.exec(l);
    if (m && !opened && /cli|device|approve|code/i.test(m[1])) {
      opened = true;
      popup(m[1]);
    }
  };
  child.stdout?.on("data", (d: Buffer) => split[0].push(d.toString()).forEach(onLine));
  child.stderr?.on("data", (d: Buffer) => split[1].push(d.toString()).forEach(onLine));
  child.on("close", (code) => {
    send("account:login", { done: true, ok: code === 0 });
    refreshStatus();
    chatView?.webContents.reload();
  });
}

// ── IPC ────────────────────────────────────────────────────────────────

type Handler = (...args: never[]) => unknown;
const handlers: Record<string, Handler> = {
  "app:info": () => ({
    version: VERSION,
    platform: process.platform,
    arch: process.arch,
    cli: cliLoc,
    appImage: process.env.APPIMAGE ?? null,
    noAutoUpdate: process.env.C0MPUTE_NO_AUTO_UPDATE === "1",
    mcpDir: defaultMcpDir(),
    skillsDir: defaultSkillsDir(),
    chatJson: chatJsonPath(),
    settings,
  }),
  "app:checkUpdate": async () => checkForUpdate({ version: VERSION, appImage: process.env.APPIMAGE }),
  "app:openExternal": (url: string) => {
    if (/^https?:\/\//.test(url)) shell.openExternal(url);
  },
  "app:openPath": (p: string) => shell.openPath(p),
  "settings:set": (patch: Partial<Settings>) => {
    const allowed: (keyof Settings)[] = ["shellEnabled", "skillScriptsAllowed", "agentModel", "worker"];
    for (const k of allowed) if (k in patch) (settings as unknown as Record<string, unknown>)[k] = patch[k];
    persist();
    if (patch.agentModel !== undefined) agent.model = patch.agentModel || undefined;
    return settings;
  },

  "chat:bounds": (b: { x: number; y: number; width: number; height: number; visible: boolean }) => {
    const v = ensureChatView();
    if (!v) return;
    v.setBounds({ x: Math.round(b.x), y: Math.round(b.y), width: Math.max(0, Math.round(b.width)), height: Math.max(0, Math.round(b.height)) });
    v.setVisible(b.visible);
  },
  "chat:reload": () => chatView?.webContents.loadURL(chatUrl()),
  "chat:back": () => chatView?.webContents.navigationHistory.goBack(),

  "cli:installShim": () => {
    if (!cliLoc.path) throw new Error("no bundled CLI to link");
    if (process.platform === "win32") throw new Error("on Windows, add the app's resources\\cli folder to PATH");
    const home = process.env.HOME || homedir();
    const existing = onPath("c0mpute");
    const existingIsShim = !!existing && readFileSync(existing, "utf8").slice(0, 200).includes("c0mpute desktop app");
    const plan = planShim({ home, bundled: cliLoc.path, appImage: process.env.APPIMAGE, existing, existingIsShim });
    if (plan.action === "skip") return plan;
    if (plan.copyTo) {
      mkdirSync(join(plan.copyTo, ".."), { recursive: true });
      copyFileSync(cliLoc.path, plan.copyTo);
      chmodSync(plan.copyTo, 0o755);
    }
    mkdirSync(join(plan.shim, ".."), { recursive: true });
    writeFileSync(plan.shim, shimScript(plan.target), { mode: 0o755 });
    return plan;
  },

  "node:status": async () => {
    try {
      lastStatus = await nodeMgr.status();
      updateTray();
      return { cli: cliLoc, status: lastStatus };
    } catch (e) {
      return { cli: cliLoc, error: e instanceof Error ? e.message : String(e) };
    }
  },
  "node:start": async () => {
    const out = await nodeMgr.start(settings.worker);
    await refreshStatus();
    return out;
  },
  "node:stop": async () => {
    const out = await nodeMgr.stop();
    await refreshStatus();
    return out;
  },
  "node:installService": () => nodeMgr.installService(),
  "node:setName": (n: string) => nodeMgr.setName(n),
  "node:setPrivate": (on: boolean) => nodeMgr.setPrivate(on),
  "node:checkUpdate": () => nodeMgr.checkUpdate(),
  "node:update": () => {
    if (cliLoc.source === "bundled") throw new Error("this CLI ships inside the app: it updates with the app (Settings > Check for updates)");
    return nodeMgr.update();
  },
  "node:logs": async () => (lastStatus ? nodeMgr.logs(lastStatus) : []),
  "node:config": async () => nodeMgr.readConfig(lastStatus ?? (await nodeMgr.status())),
  "node:saveSettings": async (s: Record<string, unknown>) => nodeMgr.saveSettings(lastStatus ?? (await nodeMgr.status()), s),
  "node:saveRawConfig": async (t: string) => nodeMgr.saveRawConfig(lastStatus ?? (await nodeMgr.status()), t),
  "node:gateway": async () => {
    const st = lastStatus ?? (await nodeMgr.status());
    const [storage, sites] = await Promise.all([
      nodeMgr.gateway<Record<string, unknown>>(st, "/storage/v1/status"),
      nodeMgr.gateway<{ sites: unknown[] }>(st, "/hosting/v1/sites"),
    ]);
    return { storage, held: sites ? sites.sites.length : null };
  },

  // null (not an error) when the local gateway is down: the worker is stopped.
  "hosting:mySites": async () => nodeMgr.mySites(lastStatus ?? (await nodeMgr.status())).catch(() => null),
  "hosting:jobs": () => hosting.list(),
  "hosting:pickDir": async () => {
    const r = await dialog.showOpenDialog(win!, { properties: ["openDirectory"] });
    return r.canceled ? null : r.filePaths[0];
  },
  "hosting:publish": (dir: string, name: string, disks: string | null) => hosting.publish(dir, name, disks),
  "hosting:rm": (name: string) => hosting.rm(name),
  "hosting:cancel": (id: string) => hosting.cancel(id),
  "connect:state": () => bridge.state(),
  "connect:set": (on: boolean, name: string) => {
    bridge.setEnabled(on, name);
    settings.connectHosting = { enabled: on, name };
    persist();
    return bridge.state();
  },

  "account:state": async () => {
    const creds = readCreds();
    let me: Me | null = null;
    let error: string | undefined;
    if (creds.key) {
      try {
        me = await api<Me>("/api/v1/me");
        lastMe = me;
      } catch (e) {
        error = e instanceof Error ? e.message : String(e);
      }
    }
    return { url: creds.url, source: creds.source, signedIn: !!creds.key, me, error };
  },
  "account:login": () => login(),
  "account:logout": async () => {
    const r = await cli.run(["chat", "logout"]);
    chatView?.webContents.reload();
    return cliOutput(r);
  },
  "account:nodes": () => api("/api/v1/hosting/nodes"),
  // Chat plugins (remote MCP servers the account's chats use): the same
  // c0mpute.com endpoints as the web chat and the CLI. Sign-in opens in an
  // app window on a one-time link, so no cookie or key is handed over.
  "account:plugins": () => api("/api/v1/chat/plugins"),
  "account:pluginAdd": async (target: string) => {
    const t = String(target ?? "").trim();
    const body = /^https?:\/\//i.test(t) ? { url: t } : { slug: t.toLowerCase() };
    const r = await api<{ plugin: { slug: string }; connect_url: string | null }>("/api/v1/chat/plugins", { method: "POST", body: JSON.stringify(body), timeoutMs: 60_000 });
    if (r.connect_url) popup(r.connect_url);
    return r;
  },
  "account:pluginConnect": async (slug: string) => {
    const r = await api<{ connect_url: string }>(`/api/v1/chat/plugins/${encodeURIComponent(slug)}/connect`);
    if (r.connect_url) popup(r.connect_url);
    return r;
  },
  "account:pluginToggle": (slug: string, enabled: boolean) =>
    api(`/api/v1/chat/plugins/${encodeURIComponent(slug)}/toggle`, { method: "POST", body: JSON.stringify({ enabled: !!enabled }) }),
  "account:pluginRemove": (slug: string) => api(`/api/v1/chat/plugins/${encodeURIComponent(slug)}`, { method: "DELETE" }),
  "account:pushes": () => api("/api/v1/hosting/pushes"),

  "tasks:media": () =>
    settings.mediaJobs.map((j) => mediaState.get(j.id) ?? { ...j, status: "unknown" }).slice(0, 50),
  "tasks:addMedia": (kind: MediaJob["kind"], id: string) => {
    if (!["images", "music", "album", "promo"].includes(kind) || !/^[A-Za-z0-9_-]{6,80}$/.test(id)) throw new Error("bad job");
    settings.mediaJobs.unshift({ kind, id, at: Date.now() });
    persist();
    pollMedia();
  },

  "agent:send": async (text: string) => {
    agentAbort = new AbortController();
    agent.send(text, agentAbort.signal);
  },
  "agent:stop": () => agentAbort?.abort(),
  "agent:reset": () => {
    agent.reset();
    agent.allowed.clear();
  },
  "agent:confirmReply": (id: string, d: Decision) => {
    const r = pendingConfirms.get(id);
    pendingConfirms.delete(id);
    r?.(d);
  },
  "agent:models": () => models(),
  "agent:tools": async () => buildTools({ mcp, skills: currentSkills(), settings }).map((t) => ({ name: t.name, source: t.source, confirm: t.confirm })),

  "mcp:list": () => mcpView(),
  "mcp:reload": async () => {
    await reloadMcp();
    return mcpView();
  },
  "mcp:save": async (server: McpServer, previousName?: string) => {
    const prev = previousName ? mcpServers.find((s) => s.name === previousName) : undefined;
    saveServer(defaultMcpDir(), { ...server, file: prev?.file ?? "" }, prev);
    await reloadMcp();
    return mcpView();
  },
  "mcp:delete": async (name: string) => {
    const s = mcpServers.find((x) => x.name === name);
    if (s) deleteServer(s);
    settings.mcpAllowed = settings.mcpAllowed.filter((n) => n !== name);
    persist();
    await reloadMcp();
    return mcpView();
  },
  "mcp:toggle": async (name: string, enabled: boolean) => {
    const s = mcpServers.find((x) => x.name === name);
    if (!s) throw new Error(`no server ${name}`);
    saveServer(defaultMcpDir(), { ...s, disabled: !enabled }, s);
    await reloadMcp();
    return mcpView();
  },
  "mcp:allow": (name: string, on: boolean) => {
    settings.mcpAllowed = settings.mcpAllowed.filter((n) => n !== name);
    if (on) settings.mcpAllowed.push(name);
    persist();
    return mcpView();
  },
  "mcp:reconnect": async (name: string) => {
    const s = mcpServers.find((x) => x.name === name);
    if (s) await mcp.connect(s);
    return mcpView();
  },
  "mcp:setSecret": async (name: string, value: string) => {
    const r = setSecret(name, value);
    await reloadMcp();
    return r;
  },
  "mcp:claudePreview": () => readClaudeServers(claudeConfigPaths(), defaultMcpDir()).map((s) => ({ ...maskServer(s), exists: mcpServers.some((x) => x.name === s.name) })),
  "mcp:claudeImport": async (names: string[]) => {
    const all = readClaudeServers(claudeConfigPaths(), defaultMcpDir()).filter((s) => names.includes(s.name));
    const r = importServers(defaultMcpDir(), all, mcpServers);
    await reloadMcp();
    return r;
  },

  "skills:list": () => ({ dir: defaultSkillsDir(), skills: currentSkills(), shellEnabled: settings.shellEnabled, scriptsAllowed: settings.skillScriptsAllowed }),
  "skills:toggle": (name: string, enabled: boolean) => {
    settings.skillsDisabled = settings.skillsDisabled.filter((n) => n !== name);
    if (!enabled) settings.skillsDisabled.push(name);
    persist();
  },
  "skills:create": (name: string, description: string) => {
    mkdirSync(defaultSkillsDir(), { recursive: true });
    const dir = createSkill(defaultSkillsDir(), name, description);
    shell.openPath(join(dir, "SKILL.md"));
    return dir;
  },
  "skills:import": async (kind: "folder" | "zip") => {
    mkdirSync(defaultSkillsDir(), { recursive: true });
    const r = await dialog.showOpenDialog(win!, kind === "folder" ? { properties: ["openDirectory"] } : { properties: ["openFile"], filters: [{ name: "Skill zip", extensions: ["zip", "skill"] }] });
    if (r.canceled || !r.filePaths[0]) return null;
    const p = r.filePaths[0];
    return kind === "folder" ? importSkillFolder(defaultSkillsDir(), p) : importSkillZip(defaultSkillsDir(), readFileSync(p), basename(p).replace(/\.(zip|skill)$/i, "").toLowerCase());
  },
  "skills:open": (name: string) => {
    const s = currentSkills().find((x) => x.name === name || basename(x.dir) === name);
    if (s) shell.openPath(s.file);
  },
};

function registerIpc() {
  for (const [ch, fn] of Object.entries(handlers)) {
    ipcMain.handle(ch, async (_e, ...args) => {
      try {
        return { ok: true, value: await (fn as (...a: unknown[]) => unknown)(...args) };
      } catch (e) {
        return { ok: false, error: e instanceof Error ? e.message : String(e) };
      }
    });
  }
  hosting.on("update", (job) => send("hosting:update", job));
  bridge.on("update", (s) => send("connect:update", s));
}

// ── lifecycle ──────────────────────────────────────────────────────────

app.on("second-instance", () => showWindow());

app.whenReady().then(async () => {
  registerIpc();
  watchChatSession();
  createWindow();
  createTray();
  refreshStatus();
  setInterval(refreshStatus, 30_000);
  setInterval(pollMedia, 10_000);
  for (const j of settings.mediaJobs) mediaState.set(j.id, { ...(j as { kind: MediaJob["kind"]; id: string; at: number }), status: "running" });
  if (settings.connectHosting.enabled) bridge.setEnabled(true, settings.connectHosting.name);
  reloadMcp().catch(() => {});
  if (!isDev) {
    setTimeout(async () => {
      const r = await checkForUpdate({ version: VERSION, appImage: process.env.APPIMAGE });
      if (r.kind === "installed") {
        const choice = await dialog.showMessageBox({ type: "info", buttons: ["Later", "Restart now"], defaultId: 1, message: `c0mpute ${r.version} is installed.`, detail: "Restart to use it. Your node keeps running." });
        if (choice.response === 1) {
          app.relaunch();
          quitting = true;
          app.quit();
        }
      } else if (r.kind === "available") {
        new Notification({ title: "c0mpute update", body: `${r.version} is out: ${r.url}` }).show();
      }
    }, 60_000);
  }
});

app.on("window-all-closed", () => {
  if (!tray) app.quit();
});

app.on("before-quit", () => {
  quitting = true;
  bridge.stop();
  hosting.stopAll();
  mcp.closeAll();
});

app.on("activate", () => showWindow());

