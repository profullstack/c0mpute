// Parses `c0mpute hosting publish` output into a progress bar.
//
// The CLI draws its bar on stderr (node/crates/c0mpute-core/src/hosting/mod.rs,
// `Progress`): on a terminal it redraws one line with `\r\x1b[2K`, otherwise it
// prints a plain line every 15 s. The lines this understands:
//
//   "  hashing     [######..] 1.20/4.00 GiB  310 MB/s"
//   "  replicating [###.....]  37.5%  1.50/12.00 GiB of 3 copies, groups 2/9 complete, ..."
//   "  2/3 network copies"            (small, unstriped sites)
//   "  2/3 network copies (1/3 preferred)"
//
// and the summary on stdout ("site:    <id>", "url:     <url>", "copies:  3/3").

export type PublishPhase = "starting" | "hashing" | "replicating" | "copies" | "done" | "failed" | "info";

export interface PublishProgress {
  phase: PublishPhase;
  /** 0..1 for the phase. */
  fraction?: number;
  /** 0..1 over the whole publish (hashing is the first 30%). */
  overall?: number;
  copies?: number;
  target?: number;
  text: string;
}

const ANSI = /\x1b\[[0-9;?]*[A-Za-z]/g;

export function stripAnsi(s: string): string {
  return s.replace(ANSI, "");
}

/** Accumulates child-process chunks and hands back complete lines (CR or LF). */
export class LineSplitter {
  private rest = "";
  push(chunk: string): string[] {
    const text = this.rest + chunk;
    const parts = text.split(/\r\n|\r|\n/);
    this.rest = parts.pop() ?? "";
    return parts.map((l) => stripAnsi(l).trim()).filter(Boolean);
  }
  flush(): string[] {
    const l = stripAnsi(this.rest).trim();
    this.rest = "";
    return l ? [l] : [];
  }
}

const HASH_WEIGHT = 0.3;

/** A tracing log line ("2026-11-11T00:12:45.19Z DEBUG c0mpute_net::swarm: ..."), not progress. */
export function isLogLine(line: string): boolean {
  return /^\d{4}-\d\d-\d\dT[\d:.]+Z?\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s/.test(stripAnsi(line).trim());
}

export function parseProgressLine(line: string): PublishProgress | null {
  const l = stripAnsi(line).trim();
  if (!l || isLogLine(l)) return null;
  let m = /^hashing\s+\[[#.]*\]\s+([\d.]+)\/([\d.]+)\s+GiB/.exec(l);
  if (m) {
    const total = Number(m[2]);
    const fraction = total > 0 ? Math.min(1, Number(m[1]) / total) : 1;
    return { phase: "hashing", fraction, overall: fraction * HASH_WEIGHT, text: l };
  }
  m = /^replicating\s+\[[#.]*\]\s+([\d.]+)%/.exec(l);
  if (m) {
    const fraction = Math.min(1, Number(m[1]) / 100);
    const target = /of (\d+) copies/.exec(l);
    return {
      phase: "replicating",
      fraction,
      overall: HASH_WEIGHT + fraction * (1 - HASH_WEIGHT),
      target: target ? Number(target[1]) : undefined,
      text: l,
    };
  }
  m = /^(\d+)\/(\d+) network copies/.exec(l);
  if (m) {
    const copies = Number(m[1]);
    const target = Number(m[2]);
    const fraction = target > 0 ? Math.min(1, copies / target) : 0;
    return { phase: "copies", fraction, copies, target, overall: HASH_WEIGHT + fraction * (1 - HASH_WEIGHT), text: l };
  }
  m = /^copies:\s+(\d+)\/(\d+)/.exec(l);
  if (m) {
    const copies = Number(m[1]);
    const target = Number(m[2]);
    return { phase: "copies", copies, target, fraction: target ? copies / target : 0, overall: 1, text: l };
  }
  if (/^(error|Error):/.test(l)) return { phase: "failed", text: l };
  return { phase: "info", text: l };
}

/** The site id and link a finished publish printed on stdout. */
export function parsePublishSummary(stdout: string): { site?: string; url?: string; copies?: number; version?: number } {
  const out: { site?: string; url?: string; copies?: number; version?: number } = {};
  for (const raw of stdout.split(/\r?\n/)) {
    const l = stripAnsi(raw).trim();
    let m = /^site:\s+([0-9a-f]{8,})/.exec(l);
    if (m) out.site = m[1];
    m = /^url:\s+(\S+)/.exec(l);
    if (m) out.url = m[1];
    m = /^copies:\s+(\d+)\//.exec(l);
    if (m) out.copies = Number(m[1]);
    m = /^version:\s+(\d+)/.exec(l);
    if (m) out.version = Number(m[1]);
  }
  return out;
}
