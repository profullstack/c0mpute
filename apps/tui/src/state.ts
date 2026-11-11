/**
 * The whole TUI as one value. Views are pure functions of it (see views/),
 * the controller is the only thing that changes it, and tests build one by
 * hand with `initialState`.
 */
import type { TextEditState } from "@profullstack/hqtui";
import type { BenchState } from "./bench.ts";
import type { Creds, Settings } from "./config.ts";
import type { Job } from "./jobs.ts";
import { emptyNode, type NodeSnapshot } from "./node.ts";
import { idle, type Loadable, type Me, type NodesPayload, type PricingPayload, type StatusPayload } from "./network.ts";

export const TABS = ["chat", "jobs", "node", "network", "settings"] as const;
export type Tab = (typeof TABS)[number];
export const TAB_LABELS: Record<Tab, string> = { chat: "Chat", jobs: "Jobs", node: "Node", network: "Network", settings: "Settings" };

export interface ChatMsg {
  role: "user" | "assistant" | "note";
  content: string;
  /** A generation turn: its live state is the job's. */
  jobId?: string;
  error?: boolean;
  at: number;
}

export interface Convo {
  id: string;
  title: string;
  /** null = the default model (Settings), or any live model. */
  model: string | null;
  messages: ChatMsg[];
}

export interface ChatState {
  convo: Convo;
  input: TextEditState;
  /** Keys go to the input. Esc leaves it, so letters work as shortcuts. */
  typing: boolean;
  streaming: boolean;
  /**
   * Lines sent while an answer (and its plugin tool calls) was still running.
   * They go out in order, one per finished answer; Esc stops the answer and
   * the next one goes at once.
   */
  queued: string[];
  /** Lines scrolled up from the bottom of the transcript. */
  scroll: number;
  /** Lines typed before, newest last, for ↑/↓. */
  history: string[];
  historyAt: number | null;
}

export interface LoginFlow {
  link: string;
  code: string;
  status: string;
  done: boolean;
}

export type SettingsRow = "account" | "model" | "downloads" | "adult" | "preview" | "pushNode";
export const SETTINGS_ROWS: SettingsRow[] = ["account", "model", "downloads", "adult", "preview", "pushNode"];

export interface AppState {
  tab: Tab;
  help: boolean;
  /** Model picker: index into ["any live model", ...models]. */
  picker: { selected: number; target: "chat" | "default" } | null;
  /** A job's image drawn in the terminal (kitty graphics), or why it cannot be. */
  preview: { jobId: string; path: string; imageId: number | null; note: string | null; cols: number; rows: number } | null;
  toast: { text: string; kind: "info" | "error" | "ok"; at: number } | null;
  creds: Creds;
  settings: Settings;
  me: Loadable<Me>;
  models: Loadable<string[]>;
  chat: ChatState;
  jobs: Job[];
  jobSel: number;
  node: NodeSnapshot;
  bench: BenchState;
  nodePanel: "roles" | "bench";
  nodeScroll: number;
  net: { status: Loadable<StatusPayload>; nodes: Loadable<NodesPayload>; pricing: Loadable<PricingPayload> };
  netPanel: "nodes" | "prices";
  netScroll: number;
  settingsSel: number;
  /** Editing the downloads dir in place. */
  editing: TextEditState | null;
  login: LoginFlow | null;
  /** In-terminal images: kitty placeholders work here. */
  imagesInTerminal: boolean;
  /** Terminal size from the last frame (for paging). */
  rows: number;
  now: number;
}

export function newConvo(model: string | null, now: number): Convo {
  return { id: `c${now.toString(36)}`, title: "new chat", model, messages: [] };
}

export function initialState(parts: {
  creds: Creds;
  settings: Settings;
  bench: BenchState;
  jobs?: Job[];
  convo?: Convo | null;
  now?: number;
  imagesInTerminal?: boolean;
}): AppState {
  const now = parts.now ?? Date.now();
  return {
    tab: "chat",
    help: false,
    picker: null,
    preview: null,
    toast: null,
    creds: parts.creds,
    settings: parts.settings,
    me: idle(),
    models: idle(),
    chat: {
      convo: parts.convo ?? newConvo(null, now),
      input: { value: "", cursor: 0 },
      typing: true,
      streaming: false,
      queued: [],
      scroll: 0,
      history: [],
      historyAt: null,
    },
    jobs: parts.jobs ?? [],
    jobSel: 0,
    node: emptyNode(),
    bench: parts.bench,
    nodePanel: "roles",
    nodeScroll: 0,
    net: { status: idle(), nodes: idle(), pricing: idle() },
    netPanel: "nodes",
    netScroll: 0,
    settingsSel: 0,
    editing: null,
    login: null,
    imagesInTerminal: parts.imagesInTerminal ?? false,
    rows: 24,
    now,
  };
}

/** The model a new message goes to: the chat's, else the default, else any. */
export function activeModel(s: AppState): string | null {
  return s.chat.convo.model ?? s.settings.defaultModel;
}

/** 18+ is on only when this terminal asked for it AND the account attested (the web rule). */
export function adultActive(s: AppState): boolean {
  return s.settings.adultMode && !!s.me.data?.adult_verified_at;
}

export function jobById(s: AppState, id: string | undefined): Job | undefined {
  return id ? s.jobs.find((j) => j.id === id) : undefined;
}
