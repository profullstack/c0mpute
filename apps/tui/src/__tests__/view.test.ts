import { describe, expect, it } from "bun:test";
import { renderToText } from "@profullstack/hqtui";
import { dataDir } from "../paths.ts";
import {
  humanRate,
  parseBenchReport,
  summarizeWorkloads,
  workerStatusFrom,
  type BenchReport,
  type State,
} from "../state.ts";
import { c0mputeTheme } from "../theme.ts";
import { renderDashboard } from "../view.ts";

const SIZE = { width: 140, height: 40, theme: c0mputeTheme };

function sample(threads: number, durationUs: number, single: number, throughput: number) {
  return {
    params: "n=34",
    threads,
    result: {
      duration: `${durationUs} us`,
      duration_us: durationUs,
      scaled: 1,
      speedup: single / durationUs,
      throughput,
    },
  };
}

const report: BenchReport = {
  metadata: {
    start_time: "2026-09-12T19:00:00Z",
    cpu: "AMD Ryzen 9 5900X 12-Core Processor",
    cores: 24,
    kernel: "Linux 7.0.0-30-generic",
    compiler: "rustc 1.92.0",
    version: "0.2.25",
    quick: false,
    elapsed_ms: 8123,
  },
  results: {
    c0mpute: {
      fib: [sample(1, 4_000_000, 4_000_000, 4e8), sample(8, 600_000, 4_000_000, 2.6e9), sample(24, 250_000, 4_000_000, 6.4e9)],
      matmul: [sample(1, 130_000, 130_000, 2e9), sample(24, 12_000, 130_000, 2.2e10)],
      hash: [sample(1, 45_000, 45_000, 1.5e9), sample(24, 9_000, 45_000, 7.4e9)],
    },
  },
  score: 9_871,
};

function state(overrides: Partial<State> = {}): State {
  return {
    worker: { kind: "running", pid: 4242 },
    bench: report,
    benchPath: "/home/op/.local/share/c0mpute/bench.json",
    benchError: null,
    modules: ["transcode", "coinpay", "infernet"],
    loadedAt: new Date(2026, 8, 12, 12, 34, 56),
    ...overrides,
  };
}

describe("renderDashboard", () => {
  it("shows the score, every workload, the worker pid and the key hints", () => {
    const text = renderToText(({ ui, theme }) => renderDashboard(ui, state(), theme), SIZE);
    expect(text).toContain("C0MPUTE");
    expect(text).toContain("WORKER pid 4242");
    expect(text).toContain("score 9,871");
    expect(text).toContain("fib (n=34)");
    expect(text).toContain("matmul");
    expect(text).toContain("hash");
    expect(text).toContain("6.40 Gvis/s @ 24 thr");
    expect(text).toContain("22.00 GFLOP/s");
    expect(text).toContain("6.89 GiB/s");
    expect(text).toContain("AMD Ryzen 9 5900X");
    expect(text).toContain("transcode");
    expect(text).toContain("reload");
    expect(text).toContain("quit");
  });

  it("tells the operator how to get a score when there is none", () => {
    const text = renderToText(
      ({ ui, theme }) => renderDashboard(ui, state({ bench: null, worker: { kind: "stopped" } }), theme),
      SIZE,
    );
    expect(text).toContain("no bench score");
    expect(text).toContain("run `c0mpute bench`");
    expect(text).toContain("/home/op/.local/share/c0mpute/bench.json");
    expect(text).toContain("WORKER stopped");
    expect(text).toContain("c0mpute worker register");
  });

  it("flags a quick run and a stale pid", () => {
    const quick: BenchReport = { ...report, metadata: { ...report.metadata, quick: true } };
    const text = renderToText(
      ({ ui, theme }) => renderDashboard(ui, state({ bench: quick, worker: { kind: "stale", pid: 99 } }), theme),
      SIZE,
    );
    expect(text).toContain("quick run, not advertised");
    expect(text).toContain("stale pid");
  });

  it("surfaces a corrupt report instead of hiding it", () => {
    const text = renderToText(
      ({ ui, theme }) =>
        renderDashboard(ui, state({ bench: null, benchError: "bench.json is not a valid report" }), theme),
      SIZE,
    );
    expect(text).toContain("bench.json is not a valid report");
  });

  it("fits a narrow terminal without throwing", () => {
    const text = renderToText(({ ui, theme }) => renderDashboard(ui, state(), theme), { ...SIZE, width: 80, height: 24 });
    expect(text).toContain("BENCH");
  });
});

describe("state helpers", () => {
  it("summarises each workload with its best sample and scaling efficiency", () => {
    const s = summarizeWorkloads(report);
    expect(s.map((x) => x.name)).toEqual(["fib", "matmul", "hash"]);
    expect(s[0]!.best.threads).toBe(24);
    expect(s[0]!.efficiency).toBeCloseTo(16 / 24, 5);
    expect(s[0]!.speedups).toHaveLength(3);
  });

  it("formats rates like the CLI", () => {
    expect(humanRate("hash", 7.4e9)).toBe("6.89 GiB/s");
    expect(humanRate("hash", 4e8)).toBe("381 MiB/s");
    expect(humanRate("matmul", 2.2e10)).toBe("22.00 GFLOP/s");
    expect(humanRate("fib", 4e8)).toBe("400 Mvis/s");
    expect(humanRate("fib", 6.4e9)).toBe("6.40 Gvis/s");
  });

  it("rejects malformed reports", () => {
    expect(parseBenchReport("not json")).toBeNull();
    expect(parseBenchReport("{}")).toBeNull();
    expect(parseBenchReport(JSON.stringify(report))?.score).toBe(9_871);
  });

  it("derives worker status from the pid file", () => {
    expect(workerStatusFrom(null, () => true)).toEqual({ kind: "stopped" });
    expect(workerStatusFrom("garbage", () => true)).toEqual({ kind: "stopped" });
    expect(workerStatusFrom("4242\n", () => true)).toEqual({ kind: "running", pid: 4242 });
    expect(workerStatusFrom("4242", () => false)).toEqual({ kind: "stale", pid: 4242 });
  });

  it("resolves the data dir the way the Rust node does", () => {
    expect(dataDir({ platform: "linux", home: "/home/op" })).toBe("/home/op/.local/share/c0mpute");
    expect(dataDir({ platform: "linux", home: "/home/op", xdgDataHome: "/data" })).toBe("/data/c0mpute");
    expect(dataDir({ platform: "darwin", home: "/Users/op" })).toBe(
      "/Users/op/Library/Application Support/com.c0mpute.c0mpute",
    );
    expect(dataDir({ platform: "win32", home: "C:\\Users\\op", appData: "C:\\Users\\op\\AppData\\Roaming" })).toContain(
      "c0mpute",
    );
  });
});
