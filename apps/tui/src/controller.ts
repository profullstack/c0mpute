/**
 * The controller: the only code that changes AppState. It runs every effect
 * (HTTP, subprocesses, files, timers) asynchronously and calls `invalidate`
 * when something arrived, so a slow network or a hung `journalctl` never
 * blocks a keypress.
 */
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { hostname } from "node:os";
import { kittyVirtualImage, type TextEditState } from "@profullstack/hqtui";
import { Api, ApiError, errorMessage, LOGIN_HINT, type SyncConvo, type SyncSummary } from "./api.ts";
import { loadBench } from "./bench.ts";
import { parseLine, HELP_COMMANDS, type Command } from "./commands.ts";
import { expandHome, loadCreds, paths, saveCreds, saveSettings, writeJson, type PreviewMode } from "./config.ts";
import { JobRunner, KIND_LABEL, startBody, type Job, type JobKind } from "./jobs.ts";
import {
  c0mputeBin,
  jobTimesDir,
  probeLatest,
  probeLogs,
  probeStatus,
  probeVersion,
  probeWorker,
  readJobTimes,
} from "./node.ts";
import { fetchMe, fetchModels, fetchNodes, fetchPricing, fetchStatus, load } from "./network.ts";
import { activeModel, adultActive, jobById, newConvo, SETTINGS_ROWS, TABS, type AppState, type ChatMsg, type Convo, type Tab } from "./state.ts";
import { textHistory } from "./web/images.ts";
import { collectPushFiles, type PushCommand, type ConnectCommand, type PushMsg } from "./web/push-shared.ts";

export interface ControllerDeps {
  api: Api;
  invalidate: () => void;
  /** Raw terminal output (image uploads). */
  write?: (seq: string) => void;
  /** Persist to ~/.c0mpute (off in tests). */
  persist?: boolean;
  now?: () => number;
  /** Start the background probes (off in tests). */
  probes?: boolean;
}

const PREVIEW_ID = 211;

export class Controller {
  readonly runner: JobRunner;
  private timers: ReturnType<typeof setInterval>[] = [];
  private abort: AbortController | null = null;
  private saveTimer: ReturnType<typeof setTimeout> | null = null;
  private busy = new Set<string>();

  constructor(
    public state: AppState,
    private readonly deps: ControllerDeps,
  ) {
    this.runner = new JobRunner({
      api: deps.api,
      downloadsDir: () => expandHome(this.state.settings.downloadsDir),
      onChange: (job, previousId) => this.jobChanged(job, previousId),
      now: deps.now,
    });
  }

  private now(): number {
    return this.deps.now?.() ?? Date.now();
  }

  private set(patch: Partial<AppState>): void {
    this.state = { ...this.state, ...patch };
    this.deps.invalidate();
  }

  toast(text: string, kind: "info" | "error" | "ok" = "info"): void {
    this.set({ toast: { text, kind, at: this.now() } });
  }

  // ─── lifecycle ────────────────────────────────────────────────────────

  start(): void {
    for (const job of this.state.jobs) this.runner.resume(job);
    void this.resumePending();
    if (this.deps.probes === false) return;
    // First paint happens before any of this lands.
    setTimeout(() => {
      void this.refreshAccount();
      void this.refreshModels();
      this.refreshWorker();
      void this.refreshStatus();
      void this.refreshLogs();
      void this.refreshVersion();
      void this.refreshNetwork();
    }, 0);
    const every = (ms: number, fn: () => void) => {
      const t = setInterval(fn, ms);
      t.unref?.();
      this.timers.push(t);
    };
    every(1000, () => {
      // Clocks on pending jobs and streaming answers; a cheap redraw.
      this.state = { ...this.state, now: this.now() };
      if (this.state.toast && this.now() - this.state.toast.at > 6000) this.state = { ...this.state, toast: null };
      this.deps.invalidate();
    });
    every(3000, () => this.refreshWorker());
    every(6000, () => this.state.tab === "node" && void this.refreshLogs());
    every(30_000, () => void this.refreshStatus());
    every(30_000, () => (this.state.tab === "network" ? void this.refreshNetwork() : undefined));
    every(120_000, () => (this.state.tab !== "network" ? void this.refreshNetwork() : undefined));
    every(60_000, () => void this.refreshAccount());
    every(60_000, () => void this.syncConvo());
    setTimeout(() => void this.syncConvo(), 1500);
    every(30 * 60_000, () => void this.refreshVersion());
  }

  stop(): void {
    for (const t of this.timers) clearInterval(t);
    this.abort?.abort();
    this.runner.dispose();
    this.flush();
  }

  private once(key: string, fn: () => Promise<void>): Promise<void> {
    if (this.busy.has(key)) return Promise.resolve();
    this.busy.add(key);
    return fn().finally(() => this.busy.delete(key));
  }

  // ─── persistence ──────────────────────────────────────────────────────

  private schedulePersist(): void {
    if (this.deps.persist === false) return;
    if (this.saveTimer) return;
    this.saveTimer = setTimeout(() => this.flush(), 500);
    this.saveTimer.unref?.();
  }

  flush(): void {
    if (this.saveTimer) clearTimeout(this.saveTimer);
    this.saveTimer = null;
    if (this.deps.persist === false) return;
    try {
      writeJson(paths.jobs(), this.state.jobs.slice(0, 200));
      writeJson(paths.chat(), this.state.chat.convo);
    } catch {}
  }

  // ─── tabs ─────────────────────────────────────────────────────────────

  setTab(tab: Tab): void {
    if (tab === this.state.tab) return;
    this.set({ tab, preview: null, editing: null });
    if (tab === "node") void this.refreshLogs();
    if (tab === "network" && !this.state.net.status.at) void this.refreshNetwork();
  }

  cycleTab(delta: number): void {
    const i = TABS.indexOf(this.state.tab);
    this.setTab(TABS[(i + delta + TABS.length) % TABS.length]!);
  }

  // ─── probes ───────────────────────────────────────────────────────────

  refreshWorker(): void {
    try {
      const { worker, running } = probeWorker();
      this.set({
        node: { ...this.state.node, worker, running, jobTimes: readJobTimes(jobTimesDir()) },
        bench: loadBench(),
      });
    } catch {}
  }

  refreshLogs(): Promise<void> {
    return this.once("logs", async () => {
      const { lines, source } = await probeLogs(this.state.node.worker, 60);
      this.set({ node: { ...this.state.node, logs: lines, logSource: source } });
    });
  }

  refreshStatus(): Promise<void> {
    return this.once("status", async () => {
      const bin = c0mputeBin();
      const r = await probeStatus(bin);
      this.set({ node: { ...this.state.node, cliPath: bin, status: r.status ?? this.state.node.status, statusText: r.text, statusError: r.error, statusAt: this.now() } });
    });
  }

  refreshVersion(): Promise<void> {
    return this.once("version", async () => {
      const bin = c0mputeBin();
      const [cliVersion, latest] = await Promise.all([probeVersion(bin), probeLatest()]);
      this.set({ node: { ...this.state.node, cliPath: bin, cliVersion, latest: latest ?? this.state.node.latest } });
    });
  }

  refreshNetwork(): Promise<void> {
    return this.once("network", async () => {
      const api = this.deps.api;
      const n = this.state.net;
      this.set({ net: { status: { ...n.status, loading: true }, nodes: { ...n.nodes, loading: true }, pricing: { ...n.pricing, loading: true } } });
      const [status, nodes, pricing] = await Promise.all([
        load(n.status, () => fetchStatus(api)),
        load(n.nodes, () => fetchNodes(api)),
        load(n.pricing, () => fetchPricing(api)),
      ]);
      this.set({ net: { status, nodes, pricing } });
    });
  }

  refreshAccount(): Promise<void> {
    return this.once("me", async () => {
      if (!this.deps.api.signedIn) {
        this.set({ me: { data: null, error: LOGIN_HINT, at: this.now(), loading: false } });
        return;
      }
      this.set({ me: await load(this.state.me, () => fetchMe(this.deps.api)) });
    });
  }

  refreshModels(): Promise<void> {
    return this.once("models", async () => {
      this.set({ models: await load(this.state.models, () => fetchModels(this.deps.api)) });
    });
  }

  refreshAll(): void {
    this.refreshWorker();
    void this.refreshStatus();
    void this.refreshLogs();
    void this.refreshNetwork();
    void this.refreshAccount();
    void this.refreshModels();
    void this.refreshVersion();
    this.toast("refreshing…");
  }

  // ─── chat ─────────────────────────────────────────────────────────────

  setInput(input: TextEditState): void {
    this.set({ chat: { ...this.state.chat, input, historyAt: null } });
  }

  setTyping(typing: boolean): void {
    this.set({ chat: { ...this.state.chat, typing } });
  }

  scrollChat(delta: number): void {
    this.set({ chat: { ...this.state.chat, scroll: Math.max(0, this.state.chat.scroll + delta) } });
  }

  /** ↑/↓ through what was typed before. */
  recall(delta: number): void {
    const { history, historyAt } = this.state.chat;
    if (!history.length) return;
    const at = historyAt === null ? (delta < 0 ? history.length - 1 : null) : historyAt + delta;
    if (at === null || at < 0) return;
    if (at >= history.length) {
      this.set({ chat: { ...this.state.chat, input: { value: "", cursor: 0 }, historyAt: null } });
      return;
    }
    const value = history[at]!;
    this.set({ chat: { ...this.state.chat, input: { value, cursor: value.length }, historyAt: at } });
  }

  private pushMsgs(...msgs: Omit<ChatMsg, "at">[]): number {
    const convo = this.state.chat.convo;
    const at = this.now();
    const messages = [...convo.messages, ...msgs.map((m) => ({ ...m, at }))];
    const firstUser = msgs.find((m) => m.role === "user");
    const title = convo.messages.length === 0 && firstUser ? firstUser.content.slice(0, 60) : convo.title;
    this.set({ chat: { ...this.state.chat, convo: { ...convo, title, messages, updated: at }, scroll: 0 } });
    this.schedulePersist();
    return messages.length - 1;
  }

  private patchMsg(index: number, patch: Partial<ChatMsg>): void {
    const convo = this.state.chat.convo;
    if (!convo.messages[index]) return;
    const messages = convo.messages.slice();
    messages[index] = { ...messages[index]!, ...patch };
    this.set({ chat: { ...this.state.chat, convo: { ...convo, messages, updated: this.now() } } });
    this.schedulePersist();
  }

  private note(text: string, user?: string): void {
    if (user) this.pushMsgs({ role: "user", content: user }, { role: "note", content: text });
    else this.pushMsgs({ role: "note", content: text });
  }

  /** Enter in the chat input. */
  async submit(): Promise<void> {
    const text = this.state.chat.input.value.trim();
    if (!text) return;
    const history = [...this.state.chat.history.filter((h) => h !== text), text].slice(-100);
    if (this.state.chat.streaming) {
      // Busy: hold the line and send it when this answer finishes.
      const queued = [...this.state.chat.queued, text];
      this.set({ chat: { ...this.state.chat, input: { value: "", cursor: 0 }, history, historyAt: null, queued } });
      this.toast(`queued (${queued.length}); sends when this answer finishes · Esc stops it now`, "info");
      return;
    }
    this.set({ chat: { ...this.state.chat, input: { value: "", cursor: 0 }, history, historyAt: null } });
    const cmd = parseLine(text);
    if (cmd) await this.run(cmd, text);
  }

  /** Send the next queued line, if the answer before it has finished. */
  private async drainQueue(): Promise<void> {
    const [next, ...rest] = this.state.chat.queued;
    if (next === undefined || this.state.chat.streaming) return;
    this.set({ chat: { ...this.state.chat, queued: rest } });
    const cmd = parseLine(next);
    if (cmd) await this.run(cmd, next);
    // A command that does not stream (/help, /image…) leaves the rest waiting.
    if (!this.state.chat.streaming && this.state.chat.queued.length) await this.drainQueue();
  }

  /** ↑ on an empty input with lines queued: take the last one back to edit. */
  unqueueLast(): boolean {
    const queued = this.state.chat.queued;
    const last = queued.at(-1);
    if (last === undefined) return false;
    this.set({ chat: { ...this.state.chat, queued: queued.slice(0, -1), input: { value: last, cursor: last.length }, historyAt: null } });
    return true;
  }

  async run(cmd: Command, text: string): Promise<void> {
    switch (cmd.kind) {
      case "help":
        return this.note(HELP_COMMANDS.map(([c, d]) => `${c.padEnd(40)} ${d}`).join("\n"), text);
      case "error":
        return this.note(cmd.message, text);
      case "new":
        return this.newChat();
      case "jobs":
        return this.setTab("jobs");
      case "history":
        return this.history(text);
      case "open":
        return this.open(text, cmd.target);
      case "models": {
        await this.refreshModels();
        const m = this.state.models.data ?? [];
        return this.note(m.length ? `live models (${m.length}):\n${m.join("\n")}\n\n/model <id> picks one; m opens the picker` : `no models listed (${this.state.models.error ?? "none live"})`, text);
      }
      case "model":
        if (cmd.model === null) return this.openPicker();
        this.setModel(cmd.model);
        return this.note(`model: ${cmd.model}`, text);
      case "chat":
        return this.chat(text);
      case "push":
        return this.push(text, cmd.push);
      case "connect":
        return this.connect(text, cmd.connect);
      case "image":
        return this.generate(text, "image", cmd.prompt, startBody("image", { prompt: cmd.prompt, adult: adultActive(this.state) }));
      case "video":
        return this.generate(text, "video", cmd.video.prompt, startBody("video", { prompt: cmd.video.prompt, model: cmd.video.model, adult: adultActive(this.state) }));
      case "music":
        return this.generate(text, "music", cmd.music.prompt, startBody("music", { prompt: cmd.music.prompt, lyrics: cmd.music.lyrics }));
      case "album":
        return this.generate(text, "album", cmd.album.prompt, startBody("album", { prompt: cmd.album.prompt }));
      case "promo":
        return this.generate(text, "promo", `${cmd.promo.style} promo for ${cmd.promo.url}`, startBody("promo", { prompt: cmd.promo.brief, ...cmd.promo }));
    }
  }

  private needSignIn(text: string): boolean {
    if (this.deps.api.signedIn) return false;
    this.note(`${LOGIN_HINT}.\nChat and generation run on your c0mpute.com account (CoinPay sign-in, a DID is required).`, text);
    return true;
  }

  private async chat(text: string): Promise<void> {
    if (this.needSignIn(text)) return;
    const before = textHistory(
      this.state.chat.convo.messages
        .filter((m) => m.role !== "note" && !m.error)
        .map((m) => ({ role: m.role, content: m.content, imageTurn: !!m.jobId })),
    );
    const index = this.pushMsgs({ role: "user", content: text }, { role: "assistant", content: "" });
    const messages = [...before.map(({ role, content }) => ({ role, content })), { role: "user", content: text }];
    await this.follow(index, (onDelta, signal, opts) => this.deps.api.streamChat(messages, activeModel(this.state), onDelta, signal, opts));
  }

  /**
   * Stream an answer into message `index`: a new turn, or one reattached by
   * run id. Esc stops it (and cancels the run on the server).
   */
  private async follow(
    index: number,
    go: (onDelta: (t: string) => void, signal: AbortSignal, opts: { onRun: (id: string) => void; onReset: () => void }) => Promise<string>,
    run?: string,
  ): Promise<void> {
    this.abort = new AbortController();
    this.set({ chat: { ...this.state.chat, streaming: true } });
    let acc = "";
    let runId = run;
    try {
      await go(
        (t) => {
          acc += t;
          this.patchMsg(index, { content: acc });
        },
        this.abort.signal,
        {
          onRun: (id) => {
            runId = id;
            this.patchMsg(index, { run: id });
            this.schedulePersist();
          },
          onReset: () => {
            acc = "";
            this.patchMsg(index, { content: "" });
          },
        },
      );
      this.patchMsg(index, { run: undefined, ...(acc ? {} : { content: "(empty answer)", error: true }) });
    } catch (e) {
      const stopped = e instanceof ApiError && e.kind === "aborted";
      if (stopped && runId) void this.deps.api.cancelRun(runId);
      const msg = stopped ? "stopped" : e instanceof Error ? e.message : String(e);
      this.patchMsg(index, { run: undefined, content: acc ? `${acc}\n\n// ${msg}` : msg, error: !acc || msg !== "stopped" });
    } finally {
      this.abort = null;
      this.set({ chat: { ...this.state.chat, streaming: false } });
      this.schedulePersist();
    }
    void this.syncConvo();
    await this.drainQueue();
  }

  /** After a restart: an answer that was still streaming is picked up again. */
  private async resumePending(): Promise<void> {
    const msgs = this.state.chat.convo.messages;
    const i = msgs.length - 1;
    const last = msgs[i];
    if (!last || last.role !== "assistant" || !last.run || !this.deps.api.signedIn) return;
    const run = last.run;
    await this.follow(i, (onDelta, signal, opts) => this.deps.api.resumeRun(run, onDelta, signal, opts.onReset), run);
  }

  // ─── account sync ─────────────────────────────────────────────────────

  /** This conversation in the account's shape (notes kept, local fields dropped). */
  private syncShape(): SyncConvo {
    const c = this.state.chat.convo;
    return {
      id: c.id,
      title: c.title,
      model: c.model ?? "auto",
      updated: c.updated ?? c.messages.at(-1)?.at ?? this.now(),
      messages: c.messages.map(({ role, content, jobId, error }) => ({ role, content, ...(jobId ? { jobId } : {}), ...(error ? { error } : {}) })),
    };
  }

  private adoptConvo(s: SyncConvo): void {
    const at = s.updated;
    const messages: ChatMsg[] = s.messages
      .filter((m) => m.role === "user" || m.role === "assistant" || m.role === "note")
      .map((m) => ({
        role: m.role as ChatMsg["role"],
        content: m.content,
        at,
        ...(typeof m.jobId === "string" ? { jobId: m.jobId } : {}),
        ...(m.error === true ? { error: true } : {}),
      }));
    const convo: Convo = { id: s.id, title: s.title, model: s.model === "auto" ? null : s.model, messages, updated: s.updated };
    this.set({ chat: { ...this.state.chat, convo, scroll: 0 } });
    this.schedulePersist();
  }

  /** Save this conversation to the account (and take a newer copy from it). Quiet on failure. */
  async syncConvo(): Promise<void> {
    if (!this.deps.api.signedIn || this.state.chat.streaming) return;
    const c = this.state.chat.convo;
    if (!c.messages.length) return;
    await this.once("sync", async () => {
      try {
        const r = await this.deps.api.putConversation(this.syncShape());
        if (!r.ok && r.newer && !this.state.chat.streaming && this.state.chat.convo.id === r.newer.id) this.adoptConvo(r.newer);
      } catch {
        // Offline, or sync off on this server: the next tick tries again.
      }
    });
  }

  private historyList: SyncSummary[] = [];

  private async history(text: string): Promise<void> {
    if (this.needSignIn(text)) return;
    try {
      await this.syncConvo();
      const all = (await this.deps.api.listConversations()).filter((c) => !c.deleted).slice(0, 30);
      this.historyList = all;
      if (!all.length) return this.note("no conversations on your account yet; they sync as you chat (web, TUI and CLI).", text);
      const cur = this.state.chat.convo.id;
      const lines = all.map((c, i) => `${String(i + 1).padStart(2)}. ${c.id === cur ? "* " : "  "}${new Date(c.updated).toISOString().slice(0, 16).replace("T", " ")}  ${c.title.slice(0, 60)}  (${c.id})`);
      this.note(`your conversations, newest first:\n${lines.join("\n")}\n\n/open <n> switches to one`, text);
    } catch (e) {
      this.note(`could not list conversations: ${e instanceof Error ? e.message : String(e)}`, text);
    }
  }

  private async open(text: string, target: string): Promise<void> {
    if (this.needSignIn(text)) return;
    if (this.state.chat.streaming) return this.toast("still answering; Esc stops it first", "info");
    const n = /^\d+$/.test(target) ? Number(target) : NaN;
    const id = Number.isFinite(n) ? this.historyList[n - 1]?.id : target;
    if (!id) return this.note("no such number; run /history first", text);
    try {
      await this.syncConvo();
      const c = await this.deps.api.getConversation(id);
      if (!c) return this.note(`no conversation ${id} on your account`, text);
      this.adoptConvo(c);
      this.toast(`opened: ${c.title}`, "ok");
    } catch (e) {
      this.note(`could not open ${id}: ${e instanceof Error ? e.message : String(e)}`, text);
    }
  }

  stopStreaming(): void {
    this.abort?.abort();
  }

  private async generate(text: string, kind: JobKind, prompt: string, request: Record<string, unknown>): Promise<void> {
    if (this.needSignIn(text)) return;
    const index = this.pushMsgs({ role: "user", content: text }, { role: "assistant", content: "" });
    // start() records the job (under a local id) before its first await, so
    // the turn can point at it now; jobChanged follows the id when the
    // network assigns the real one.
    const started = this.runner.start(kind, prompt, request);
    const local = this.state.jobs[0];
    if (local) this.patchMsg(index, { jobId: local.id });
    await started;
  }

  private jobChanged(job: Job, previousId: string): void {
    const jobs = this.state.jobs.slice();
    const i = jobs.findIndex((j) => j.id === previousId || j.id === job.id);
    if (i >= 0) jobs[i] = job;
    else jobs.unshift(job);
    let convo = this.state.chat.convo;
    if (previousId !== job.id && convo.messages.some((m) => m.jobId === previousId)) {
      convo = { ...convo, messages: convo.messages.map((m) => (m.jobId === previousId ? { ...m, jobId: job.id } : m)) };
    }
    const prev = this.state.jobs.find((j) => j.id === previousId);
    this.set({ jobs, chat: { ...this.state.chat, convo } });
    if (job.status === "done" && prev?.status !== "done" && job.files[0]) {
      this.toast(`${KIND_LABEL[job.kind]} saved: ${job.files[0].path}`, "ok");
    }
    this.schedulePersist();
  }

  newChat(): void {
    void this.syncConvo();
    this.abort?.abort();
    this.set({ chat: { ...this.state.chat, convo: newConvo(this.state.chat.convo.model, this.now()), scroll: 0, streaming: false, queued: [] } });
    this.schedulePersist();
  }

  setModel(model: string | null): void {
    this.set({ chat: { ...this.state.chat, convo: { ...this.state.chat.convo, model } }, picker: null });
    this.schedulePersist();
  }

  openPicker(target: "chat" | "default" = "chat"): void {
    if (!this.state.models.data) void this.refreshModels();
    const list = [null, ...(this.state.models.data ?? [])];
    const cur = Math.max(0, list.indexOf(target === "default" ? this.state.settings.defaultModel : activeModel(this.state)));
    this.set({ picker: { selected: cur, target } });
  }

  movePicker(delta: number): void {
    if (!this.state.picker) return;
    const n = (this.state.models.data?.length ?? 0) + 1;
    this.set({ picker: { ...this.state.picker, selected: Math.max(0, Math.min(n - 1, this.state.picker.selected + delta)) } });
  }

  choosePicker(index = this.state.picker?.selected ?? 0): void {
    const model = index === 0 ? null : (this.state.models.data?.[index - 1] ?? null);
    if (this.state.picker?.target === "default") return this.setDefaultModel(model);
    this.setModel(model);
    this.toast(`model: ${model ?? "any live model"}`, "ok");
  }

  closeOverlays(): boolean {
    if (this.state.help || this.state.picker || this.state.preview || this.state.editing) {
      this.set({ help: false, picker: null, preview: null, editing: null });
      return true;
    }
    return false;
  }

  toggleHelp(): void {
    this.set({ help: !this.state.help });
  }

  // ─── /push hosting and /connect hosting ───────────────────────────────

  private pushMsgsForUpload(): PushMsg[] {
    return this.state.chat.convo.messages.map((m) => {
      const job = jobById(this.state, m.jobId);
      if (!job || job.status !== "done") return { role: m.role === "note" ? "system" : m.role, content: m.content };
      const content = job.kind === "music" && job.title ? `"${job.title}" [music: ${job.prompt}]` : `[${KIND_LABEL[job.kind]}: ${job.prompt}]`;
      const base: PushMsg = { role: "assistant", content, adult: job.adult };
      if (job.kind === "image") return { ...base, imageJob: job.id };
      if (job.kind === "music") return { ...base, musicJob: job.id, audioType: job.files[0]?.type || "audio/mpeg" };
      if (job.kind === "promo") return { ...base, promo: { id: job.id, video: `/api/v1/promo/${job.id}/video`, captions: `/api/v1/promo/${job.id}/captions` } };
      if (job.kind === "album") {
        const [band, album] = (job.title ?? "").replace(/^"/, "").split(/" by /);
        return { ...base, album: { id: job.id, band: band ?? null, album: album?.split(" · ")[0] ?? null, cover: job.files.some((f) => f.label === "cover") ? "yes" : null, zip: "yes" } };
      }
      return base;
    });
  }

  private async push(text: string, cmd: PushCommand): Promise<void> {
    if (cmd.kind === "help") {
      return this.note(
        `${cmd.error ? `${cmd.error}\n\n` : ""}/push hosting <name>     publish this chat's files (images, songs, albums, videos, transcript) as a site on the c0mpute network\n/push hosting ls         your sites\n/push hosting rm <name>  take a site down\n/connect hosting [node]  link or pick the node that publishes them`,
        text,
      );
    }
    if (this.needSignIn(text)) return;
    const index = this.pushMsgs({ role: "user", content: text }, { role: "note", content: "…" });
    const say = (content: string, error = false) => this.patchMsg(index, { content, error });
    const api = this.deps.api;
    try {
      if (cmd.kind === "ls") {
        const r = await api.json<{ pushes?: { name: string; status: string; node: string | null; url: string | null; files: unknown[]; progress: string | null; error: string | null }[] }>("/api/v1/hosting/pushes");
        if (r.status !== 200) return say(`could not list: ${errorMessage(r.body, r.status)}`, true);
        const ps = r.body?.pushes ?? [];
        if (!ps.length) return say("no sites yet. /push hosting <name> publishes this chat's files.");
        return say(
          `your sites (${ps.length}):\n` +
            ps.map((p) => `${p.name.padEnd(24)} ${p.status.padEnd(10)} ${String(p.files.length).padStart(3)} files  ${p.node ?? "?"}  ${p.url ?? p.progress ?? ""}${p.error ? `\n  ! ${p.error}` : ""}`).join("\n"),
        );
      }
      if (cmd.kind === "rm") {
        say(`taking ${cmd.name} down…`);
        const r = await api.json(`/api/v1/hosting/pushes/${cmd.name}`, { method: "DELETE" });
        if (r.status >= 300) return say(errorMessage(r.body, r.status), true);
        return say(`${cmd.name}: delete queued on your hosting node (/push hosting ls shows when it is gone)`);
      }
      const files = collectPushFiles(this.state.chat.convo.title, this.pushMsgsForUpload());
      say(`sending ${files.length} files to your hosting node…`);
      const r = await api.json<{ push?: { id: string; name: string; status: string; rev: number; node: string | null; url: string | null }; poll?: string }>("/api/v1/hosting/pushes", {
        body: { name: cmd.name, node: this.state.settings.pushNode ?? undefined, files },
      });
      if (r.status !== 202 || !r.body?.push) {
        const msg = errorMessage(r.body, r.status);
        return say(`push failed: ${msg}${r.status === 409 && /no hosting node/.test(msg) ? "\n\nTry /connect hosting." : ""}`, true);
      }
      const p = r.body.push;
      say(`push hosting ${p.name} (rev ${p.rev}): ${p.status} on ${p.node ?? "your node"}…`);
      void this.pollPush(index, p.id, p.name);
    } catch (e) {
      say(e instanceof Error ? e.message : String(e), true);
    }
  }

  private async pollPush(index: number, id: string, name: string): Promise<void> {
    const until = this.now() + 2 * 3600_000;
    while (this.now() < until) {
      await new Promise((r) => setTimeout(r, 3000));
      try {
        const r = await this.deps.api.json<{ push?: { status: string; url: string | null; progress: string | null; copies: number | null; node: string | null; error: string | null } }>(`/api/v1/hosting/pushes/${id}`);
        if (r.status >= 500) continue;
        const p = r.body?.push;
        if (r.status !== 200 || !p) return this.patchMsg(index, { content: `push ${name}: ${errorMessage(r.body, r.status)}`, error: true });
        const line = `push hosting ${name}: ${p.status}${p.progress ? ` · ${p.progress}` : ""}${p.url ? `\n${p.url}` : ""}${p.copies ? ` (${p.copies} copies)` : ""}${p.error ? `\n! ${p.error}` : ""}`;
        this.patchMsg(index, { content: line, error: p.status === "failed" });
        if (p.status === "live" || p.status === "failed") return;
      } catch {}
    }
  }

  private async connect(text: string, cmd: ConnectCommand): Promise<void> {
    if (this.needSignIn(text)) return;
    const index = this.pushMsgs({ role: "user", content: text }, { role: "note", content: "looking up your hosting nodes…" });
    const say = (content: string, error = false) => this.patchMsg(index, { content, error });
    if (cmd.error) return say(cmd.error, true);
    const howTo = "Link a node: on the machine that should publish, run\n\n  c0mpute chat login\n  c0mpute chat connect hosting --name <node>";
    try {
      const r = await this.deps.api.json<{ nodes?: { name: string; online: boolean; payments: string | null; version: string | null }[] }>("/api/v1/hosting/nodes");
      if (r.status !== 200) return say(`could not list nodes: ${errorMessage(r.body, r.status)}`, true);
      const nodes = r.body?.nodes ?? [];
      let chosen = this.state.settings.pushNode;
      if (cmd.node) {
        if (!nodes.some((n) => n.name === cmd.node)) return say(`no node named ${cmd.node} on your account.\n\n${howTo}`, true);
        chosen = cmd.node;
        this.updateSettings({ pushNode: chosen });
      }
      if (!nodes.length) return say(`no hosting node linked yet.\n\n${howTo}`);
      const lines = nodes.map(
        (n) =>
          `${n.name === chosen ? "*" : " "} ${n.name.padEnd(20)} ${n.online ? "online " : "offline"}  ${n.payments === "coinpay" ? "pays via CoinPay" : "NO CoinPay payments: cannot push"}${n.version ? `  v${n.version}` : ""}`,
      );
      say(`${cmd.node ? `/push hosting now publishes through ${cmd.node}.\n\n` : ""}your hosting nodes (* = used by /push):\n${lines.join("\n")}\n\n/connect hosting <node> picks one.\n\n${howTo}`);
    } catch (e) {
      say(e instanceof Error ? e.message : String(e), true);
    }
  }

  // ─── jobs ─────────────────────────────────────────────────────────────

  moveJob(delta: number): void {
    const n = this.state.jobs.length;
    if (!n) return;
    this.set({ jobSel: Math.max(0, Math.min(n - 1, this.state.jobSel + delta)) });
  }

  selectedJob(): Job | undefined {
    return this.state.jobs[this.state.jobSel];
  }

  async retryJob(job = this.selectedJob()): Promise<void> {
    if (!job) return;
    if (!this.deps.api.signedIn) return this.toast(LOGIN_HINT, "error");
    this.toast(`retrying the ${KIND_LABEL[job.kind]}…`);
    await this.runner.start(job.kind, job.prompt, job.request);
    this.set({ jobSel: 0 });
  }

  stopJob(job = this.selectedJob()): void {
    if (!job || (job.status !== "pending" && job.status !== "starting")) return;
    this.runner.stop(job);
    this.toast("stopped watching. The network has no cancel: it may still finish (and bill) the job.", "info");
  }

  removeJob(job = this.selectedJob()): void {
    if (!job) return;
    if (job.status === "pending") this.runner.stop(job);
    const jobs = this.state.jobs.filter((j) => j.id !== job.id);
    this.set({ jobs, jobSel: Math.min(this.state.jobSel, Math.max(0, jobs.length - 1)) });
    this.schedulePersist();
  }

  /** Open the first saved file with the desktop's viewer. */
  openJob(job = this.selectedJob(), which = 0): void {
    const file = job?.files[which] ?? job?.files[0];
    if (!file) return this.toast(job ? "nothing saved yet" : "no job selected", "info");
    if (process.platform === "linux" && !process.env.DISPLAY && !process.env.WAYLAND_DISPLAY) {
      return this.toast(`no desktop here to open it; the file is at ${file.path}`, "info");
    }
    const opener = process.platform === "darwin" ? "open" : process.platform === "win32" ? "explorer" : "xdg-open";
    try {
      spawn(opener, [file.path], { detached: true, stdio: "ignore" }).on("error", () => this.toast(`${opener} not available; the file is at ${file.path}`, "error")).unref();
      this.toast(`opening ${file.path}`, "ok");
    } catch {
      this.toast(`could not open; the file is at ${file.path}`, "error");
    }
  }

  /** Draw an image result in the terminal (kitty graphics), else open it. */
  previewJob(job = this.selectedJob(), size?: { cols: number; rows: number }): void {
    const file = job?.files.find((f) => /\.(png|jpe?g)$/i.test(f.path));
    if (!job || !file) return this.toast("no image to preview (p previews images and covers)", "info");
    const mode: PreviewMode = this.state.settings.preview;
    if (mode === "off") return this.toast(`previews are off (Settings); the file is at ${file.path}`, "info");
    if (mode === "open" || !this.state.imagesInTerminal || !this.deps.write) {
      if (mode === "auto" && !this.state.imagesInTerminal) {
        this.set({ preview: { jobId: job.id, path: file.path, imageId: null, note: "this terminal cannot draw images (kitty graphics; set HQTUI_IMAGES=1 under tmux/SSH)", cols: 50, rows: 3 } });
      }
      return this.openJob(job, job.files.indexOf(file));
    }
    try {
      const png = readFileSync(file.path);
      const cols = size?.cols ?? 60;
      const rows = size?.rows ?? 18;
      this.deps.write(kittyVirtualImage(PREVIEW_ID, png, cols, rows));
      this.set({ preview: { jobId: job.id, path: file.path, imageId: PREVIEW_ID, note: null, cols, rows } });
    } catch (e) {
      this.toast(`preview failed: ${e instanceof Error ? e.message : e}`, "error");
    }
  }

  // ─── node / network panels ────────────────────────────────────────────

  toggleBench(): void {
    this.set({ nodePanel: this.state.nodePanel === "bench" ? "roles" : "bench" });
  }

  scrollNode(delta: number): void {
    this.set({ nodeScroll: Math.max(0, this.state.nodeScroll + delta) });
  }

  toggleNetPanel(): void {
    this.set({ netPanel: this.state.netPanel === "nodes" ? "prices" : "nodes", netScroll: 0 });
  }

  scrollNet(delta: number): void {
    this.set({ netScroll: Math.max(0, this.state.netScroll + delta) });
  }

  // ─── settings ─────────────────────────────────────────────────────────

  updateSettings(patch: Partial<AppState["settings"]>): void {
    const settings = { ...this.state.settings, ...patch };
    this.set({ settings });
    if (this.deps.persist !== false) {
      try {
        saveSettings(settings);
      } catch (e) {
        this.toast(`could not save settings: ${e instanceof Error ? e.message : e}`, "error");
      }
    }
  }

  moveSetting(delta: number): void {
    this.set({ settingsSel: Math.max(0, Math.min(SETTINGS_ROWS.length - 1, this.state.settingsSel + delta)) });
  }

  /** Enter on a settings row. */
  activateSetting(row = SETTINGS_ROWS[this.state.settingsSel]): void {
    switch (row) {
      case "account":
        return this.deps.api.signedIn ? void this.refreshAccount() : void this.login();
      case "model": {
        // The picker sets the default when opened from here.
        return this.openPicker("default");
      }
      case "downloads":
        return this.set({ editing: { value: this.state.settings.downloadsDir, cursor: this.state.settings.downloadsDir.length } });
      case "adult":
        return this.toggleAdult();
      case "preview": {
        const order: PreviewMode[] = ["auto", "open", "off"];
        return this.updateSettings({ preview: order[(order.indexOf(this.state.settings.preview) + 1) % order.length]! });
      }
      case "pushNode":
        return this.updateSettings({ pushNode: null });
    }
  }

  setEditing(edit: TextEditState | null): void {
    this.set({ editing: edit });
  }

  commitEditing(): void {
    const v = this.state.editing?.value.trim();
    if (v) this.updateSettings({ downloadsDir: v });
    this.set({ editing: null });
    if (v) this.toast(`downloads: ${expandHome(v)}`, "ok");
  }

  /** The 18+ switch: the web chat's rules, nothing more. */
  toggleAdult(): void {
    const verified = !!this.state.me.data?.adult_verified_at;
    if (!this.state.settings.adultMode && !verified) {
      this.toast("18+ mode needs the account's 18+ verification, done in the c0mpute.com/chat settings (an API key cannot attest)", "info");
      return;
    }
    this.updateSettings({ adultMode: !this.state.settings.adultMode });
  }

  /** "remove 18+ verification", as in the web settings. */
  async revokeAdult(): Promise<void> {
    if (!this.state.me.data?.adult_verified_at) return;
    try {
      const r = await this.deps.api.json("/api/v1/me/adult", { method: "DELETE" });
      if (r.status !== 200) return this.toast(errorMessage(r.body, r.status), "error");
      this.updateSettings({ adultMode: false });
      await this.refreshAccount();
      this.toast("18+ verification removed", "ok");
    } catch (e) {
      this.toast(e instanceof Error ? e.message : String(e), "error");
    }
  }

  /** Model picker opened from Settings: the choice becomes the default for new chats. */
  setDefaultModel(model: string | null): void {
    this.updateSettings({ defaultModel: model });
    this.set({ picker: null });
    this.toast(`default model: ${model ?? "any live model"}`, "ok");
  }

  /**
   * `c0mpute chat login`, in the TUI: the device flow. Approve the code on
   * c0mpute.com with CoinPay; the key is saved to ~/.c0mpute/chat.json,
   * where the CLI finds it too.
   */
  login(): Promise<void> {
    return this.once("login", async () => {
      const api = this.deps.api;
      try {
        const r = await api.json<{ device_code: string; user_code: string; verification_uri_complete: string; interval?: number; expires_in?: number }>("/api/v1/cli/device", {
          body: { label: `c0mpute tui on ${hostname()}` },
          auth: false,
        });
        if (r.status !== 200 || !r.body?.device_code) {
          this.set({ login: { link: "", code: "", status: `could not start sign-in: ${errorMessage(r.body, r.status)}`, done: true } });
          return;
        }
        const d = r.body;
        this.set({ login: { link: d.verification_uri_complete, code: d.user_code, status: "waiting for approval…", done: false } });
        const until = this.now() + (d.expires_in ?? 600) * 1000;
        while (this.now() < until && this.state.login && !this.state.login.done) {
          await new Promise((res) => setTimeout(res, Math.max(1, d.interval ?? 3) * 1000));
          const t = await api.json<{ key?: string }>("/api/v1/cli/token", { body: { device_code: d.device_code }, auth: false }).catch(() => null);
          if (!t || t.status === 428 || t.status === 429) continue;
          if (t.status === 200 && t.body?.key) {
            const path = saveCreds(api.creds.url, t.body.key);
            api.creds = loadCreds();
            this.set({ creds: api.creds, login: { ...this.state.login!, status: `signed in; key saved to ${path}`, done: true } });
            await this.refreshAccount();
            return;
          }
          this.set({ login: { ...this.state.login!, status: t.status === 410 ? "the code expired; press L to try again" : errorMessage(t.body, t.status), done: true } });
          return;
        }
        if (this.state.login && !this.state.login.done) this.set({ login: { ...this.state.login, status: "the code expired; press L to try again", done: true } });
      } catch (e) {
        this.set({ login: { link: "", code: "", status: e instanceof Error ? e.message : String(e), done: true } });
      }
    });
  }

  cancelLogin(): void {
    if (this.state.login) this.set({ login: null });
  }
}
