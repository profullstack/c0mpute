// What the c0mpute network observed being paid, for /estimates: every node's
// completed jobs as the status aggregators heard them, grouped by hardware
// class and workload. Pure: rows in, summary out.
//
// The money here is the agreed price in the c0mpute auction (what the buyer
// accepted for the job), less the assumed market fee: the same "agreed"
// figure the operator dashboard shows. Chat inference bridged from Infernet
// (SETTLED_IN_INFERNET_TYPES) is paid in Infernet's ledger, not at its auction
// price, so its jobs and hours are shown but carry no money here: the settled
// totals on the page count it, once.

import { operatorEarnings } from "../node-costs";
import { SETTLED_IN_INFERNET_TYPES } from "./network";

const SETTLED_ELSEWHERE = new Set<string>(SETTLED_IN_INFERNET_TYPES);

export interface NetworkJobRow {
  node: string;
  type: string;
  jobs: number;
  busy_s: number;
  paid_usd: number;
}

export interface ObservedGroup {
  key: string;
  label: string;
  nodes: number;
  jobs: number;
  busy_hours: number;
  /** Agreed auction price, before the fee. */
  gross_usd: number;
  /** After the assumed market fee. */
  net_usd: number;
  /** net_usd / priced busy hours: comparable to an hourly rental rate for paid time. Null with no priced time. */
  net_per_busy_hour: number | null;
  /** Jobs whose pay is settled in Infernet's ledger, so they carry no money here. */
  settled_elsewhere_jobs: number;
}

export interface ObservedWindow {
  days: number;
  /** Days of history actually on file inside the window (history began recently). */
  covered_days: number;
  totals: ObservedGroup;
  by_class: ObservedGroup[];
  by_type: ObservedGroup[];
}

const CLASS_LABELS: Record<string, string> = {
  cpu: "CPU",
  "gpu:nvidia": "NVIDIA GPU",
  "gpu:amd": "AMD GPU",
  "gpu:apple": "Apple silicon GPU",
  unknown: "class not on file",
};

export function classLabel(c: string): string {
  return CLASS_LABELS[c] ?? c;
}

const TYPE_LABELS: Record<string, string> = {
  "infernet.inference": "chat inference (Infernet)",
  "tts.synthesize": "text to speech",
  "stt.transcribe": "speech to text",
  "diffusion.txt2img": "image generation",
  "music.txt2music": "music generation",
};

export function typeLabel(t: string): string {
  return TYPE_LABELS[t] ?? t;
}

function group(key: string, label: string, rows: NetworkJobRow[]): ObservedGroup {
  let jobs = 0;
  let busy = 0;
  let pricedBusy = 0;
  let gross = 0;
  let elsewhere = 0;
  const nodes = new Set<string>();
  for (const r of rows) {
    jobs += r.jobs;
    busy += r.busy_s;
    nodes.add(r.node);
    if (SETTLED_ELSEWHERE.has(r.type)) {
      elsewhere += r.jobs;
      continue;
    }
    pricedBusy += r.busy_s;
    gross += r.paid_usd;
  }
  const busyHours = busy / 3600;
  const pricedHours = pricedBusy / 3600;
  const net = operatorEarnings(gross);
  return {
    key,
    label,
    nodes: nodes.size,
    jobs,
    busy_hours: busyHours,
    gross_usd: gross,
    net_usd: net,
    // Under a minute of priced time makes a per-hour figure meaningless.
    net_per_busy_hour: pricedHours >= 1 / 60 ? net / pricedHours : null,
    settled_elsewhere_jobs: elsewhere,
  };
}

function groupBy(rows: NetworkJobRow[], keyOf: (r: NetworkJobRow) => string, labelOf: (k: string) => string): ObservedGroup[] {
  const m = new Map<string, NetworkJobRow[]>();
  for (const r of rows) {
    const k = keyOf(r);
    const list = m.get(k) ?? [];
    list.push(r);
    m.set(k, list);
  }
  return [...m.entries()]
    .map(([k, list]) => group(k, labelOf(k), list))
    .sort((a, b) => b.net_usd - a.net_usd || b.jobs - a.jobs || a.key.localeCompare(b.key));
}

export function buildObserved(input: {
  days: number;
  rows: NetworkJobRow[];
  classOf: (node: string) => string | null | undefined;
  historyFrom: number | null;
  now: number;
}): ObservedWindow {
  const { days, rows, now } = input;
  const windowMs = days * 86_400_000;
  const covered = input.historyFrom == null ? 0 : Math.min(windowMs, Math.max(0, now - input.historyFrom)) / 86_400_000;
  return {
    days,
    covered_days: Math.round(covered * 10) / 10,
    totals: group("all", "all hardware", rows),
    by_class: groupBy(rows, (r) => input.classOf(r.node) ?? "unknown", classLabel),
    by_type: groupBy(rows, (r) => r.type, typeLabel),
  };
}
