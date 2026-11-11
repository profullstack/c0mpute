import { describe, expect, it } from "bun:test";
import { closeSync, existsSync, fstatSync, mkdirSync, mkdtempSync, openSync, readFileSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { errorMessage, sseParser } from "../api.ts";
import { humanRate, parseBenchReport, summarizeWorkloads } from "../bench.ts";
import { musicLine, parseLine, parseVideoCommand } from "../commands.ts";
import { loadCreds, loadSettings, saveCreds } from "../config.ts";
import { demoBench } from "../demo.ts";
import {
  applyDoneJson,
  applyPending,
  applyStarted,
  binaryResultName,
  newJob,
  slug,
  startBody,
  uniquePath,
} from "../jobs.ts";
import {
  classifyProcess,
  compareVersions,
  descendants,
  parseLogLine,
  parseStat,
  parseStatusJson,
  parseVersion,
  probeWorker,
  readJobTimes,
  tailLog,
  workerFlags,
  type ProcInfo,
} from "../node.ts";
import { dataDir } from "../paths.ts";
import { priceLabel, roleCounts, usd } from "../network.ts";

const NOW = 1_800_000_000_000;

describe("the web chat's parsers are the web chat's", () => {
  // src/web/ is a verbatim copy of apps/web/src/lib/chat (the TUI installs
  // without the web app). Keep them identical so a command means the same
  // thing in both.
  const web = join(import.meta.dir, "../../../web/src/lib/chat");
  for (const f of ["images", "music", "album", "promo", "push-shared", "videos"]) {
    it(`${f}.ts matches apps/web`, () => {
      if (!existsSync(web)) return;
      expect(readFileSync(join(import.meta.dir, `../web/${f}.ts`), "utf8")).toBe(readFileSync(join(web, `${f}.ts`), "utf8"));
    });
  }
});

describe("commands", () => {
  it("routes each slash command like the web chat", () => {
    expect(parseLine("hello there")).toEqual({ kind: "chat", text: "hello there" });
    expect(parseLine("/image a red bicycle")).toEqual({ kind: "image", prompt: "a red bicycle" });
    expect(parseLine("/draw a cat")).toEqual({ kind: "image", prompt: "a cat" });
    expect(parseLine("/album synthwave about a drowned city")).toEqual({ kind: "album", album: { prompt: "synthwave about a drowned city" } });
    expect(parseLine("/music lo-fi beats")).toEqual({ kind: "music", music: { prompt: "lo-fi beats", lyrics: "" } });
    const promo = parseLine("/promo https://c0mpute.com anime vertical");
    expect(promo?.kind).toBe("promo");
    if (promo?.kind === "promo") expect(promo.promo).toMatchObject({ url: "https://c0mpute.com", style: "anime", vertical: true });
    expect(parseLine("make a pixar promo video for c0mpute.com")?.kind).toBe("promo");
    expect(parseLine("/push hosting ls")).toEqual({ kind: "push", push: { kind: "ls", target: "hosting" } });
    expect(parseLine("/push hosting my-site")).toEqual({ kind: "push", push: { kind: "push", target: "hosting", name: "my-site" } });
    expect(parseLine("/connect hosting n1")).toEqual({ kind: "connect", connect: { target: "hosting", node: "n1" } });
    expect(parseLine("/model qwen2.5:7b")).toEqual({ kind: "model", model: "qwen2.5:7b" });
    expect(parseLine("/model")).toEqual({ kind: "model", model: null });
    expect(parseLine("/new")).toEqual({ kind: "new" });
    expect(parseLine("/help")).toEqual({ kind: "help" });
    expect(parseLine("/image")).toEqual({ kind: "error", message: "usage: /image <what to draw>" });
    expect(parseLine("/frobnicate")?.kind).toBe("error");
    expect(parseLine("   ")).toBeNull();
  });

  it("takes lyrics inline, since a terminal input is one line", () => {
    expect(musicLine("/music lo-fi lyrics: la la la")).toBe("/music lo-fi\nlyrics:\nla la la");
    expect(parseLine("/music lo-fi lyrics: la la la")).toEqual({ kind: "music", music: { prompt: "lo-fi", lyrics: "la la la" } });
  });

  it("parses /video with an optional model", () => {
    expect(parseVideoCommand("/video a paper boat")).toEqual({ prompt: "a paper boat", model: "wan2.1-t2v-1.3b" });
    expect(parseVideoCommand("/clip --model wan2.2-ti2v-5b waves")).toEqual({ prompt: "waves", model: "wan2.2-ti2v-5b" });
    expect(parseVideoCommand("/video --model nope waves")).toContain("model must be one of");
    expect(parseVideoCommand("/video")).toBe("usage: /video <what happens in the clip>");
    expect(parseVideoCommand("hello")).toBeNull();
    // /video-promo is the promo command, not a clip.
    expect(parseLine("/video-promo https://c0mpute.com")?.kind).toBe("promo");
  });
});

describe("jobs", () => {
  const job = (kind: Parameters<typeof newJob>[0]) => ({ ...newJob(kind, "a red bicycle", { prompt: "a red bicycle" }, NOW), id: "f1e2d3c4-b5a6-4789-9abc-def012345678", poll: "/x", status: "pending" as const });

  it("starts from the 202 and fails without an id", () => {
    const j = applyStarted(newJob("image", "p", {}, NOW), { id: "abc", poll: "/api/v1/images/abc" }, NOW + 1);
    expect(j).toMatchObject({ id: "abc", poll: "/api/v1/images/abc", status: "pending" });
    expect(applyStarted(newJob("image", "p", {}, NOW), {}, NOW).status).toBe("failed");
  });

  it("shows a queued image with its place in line", () => {
    const j = applyPending(job("image"), { stage: "queued", queue_position: 3 }, NOW);
    expect(j.stage).toContain("queued for a c0mpute image worker (#3 in line)");
    expect(j.queuePosition).toBe(3);
  });

  it("turns a song's node progress into stage, worker, ETA and a bar", () => {
    const j = applyPending(
      job("music"),
      { title: "Rain", lyrics: "la", node: { stage: "rendering", worker: "12D3KooWabcdefgh12345678", eta_s: 200, stage_elapsed_s: 50 } },
      NOW,
    );
    expect(j.title).toBe("Rain");
    expect(j.worker).toBe("12D3KooWabcdefgh12345678");
    expect(j.stage).toContain("rendering on node …12345678");
    expect(j.progress).toBeCloseTo(0.25, 5);
    expect(j.etaS).toBe(150);
    expect(applyPending(job("music"), { stage: "writing" }, NOW).stage).toContain("writing lyrics");
  });

  it("counts album tracks and promo stages", () => {
    const a = applyPending(
      job("album"),
      {
        stage: "recording",
        band: "Rusty Waves",
        album: "Metal Tides",
        tracks: [
          { n: 1, title: "One", status: "done" },
          { n: 2, title: "Two", status: "pending", node: { stage: "rendering", worker: "w", eta_s: 60, stage_elapsed_s: 10 } },
          { n: 3, title: "Three", status: "written" },
          { n: 4, title: "Four", status: "failed", error: "worker died" },
        ],
      },
      NOW,
    );
    expect(a.progress).toBe(0.25);
    expect(a.title).toBe('"Metal Tides" by Rusty Waves');
    expect(a.stage).toContain("1/4 done");
    expect(a.lines[0]).toBe("[x] 01 One");
    expect(a.lines[3]).toContain("[!] 04 Four · worker died");
    const p = applyPending(job("promo"), { stages: { brand: { state: "done" }, script: { state: "done" }, frames: { state: "running", detail: "4/6" } } }, NOW);
    expect(p.progress).toBeCloseTo(2 / 7, 5);
    expect(p.stage).toBe("frames: 4/6");
    expect(p.lines).toContain("[~] frames: 4/6");
  });

  it("knows what to download when a job finishes", () => {
    const v = applyDoneJson(job("video"), { status: "done", video: "/api/v1/videos/x/video" }, NOW);
    expect(v.files).toEqual([{ url: "/api/v1/videos/x/video", name: "a-red-bicycle-f1e2d3c4.mp4", label: "video" }]);
    const a = applyDoneJson({ ...job("album") }, { band: "Rusty Waves", album: "Metal Tides", zip: "/api/v1/album/f1/zip", tracks: [] }, NOW);
    expect(a.files.map((f) => f.name)).toEqual(["Rusty Waves - Metal Tides.zip", "Rusty Waves - Metal Tides.png"]);
    expect(a.files[1]!.optional).toBe(true);
    const p = applyDoneJson(job("promo"), { title: "Ship it", video: "/v", captions: "/c", stages: {} }, NOW);
    expect(p.files.map((f) => f.label)).toEqual(["video", "captions"]);
    expect(binaryResultName({ ...job("music"), title: "Rain: on/the window" }, "audio/mpeg")).toBe("Rain on the window-f1e2d3c4.mp3");
    expect(binaryResultName(job("image"), "image/png")).toBe("a-red-bicycle-f1e2d3c4.png");
  });

  it("sends the web chat's request bodies", () => {
    expect(startBody("image", { prompt: "x" })).toEqual({ prompt: "x" });
    expect(startBody("image", { prompt: "x", adult: true })).toEqual({ prompt: "x", adult: true });
    expect(startBody("video", { prompt: "x", model: "wan2.1-t2v-1.3b" })).toEqual({ prompt: "x", model: "wan2.1-t2v-1.3b" });
    expect(startBody("music", { prompt: "x" })).toEqual({ prompt: "x", lyrics: "" });
    expect(startBody("promo", { prompt: "", url: "https://a.com", style: "pixar", vertical: false, brief: "b" })).toEqual({ url: "https://a.com", style: "pixar", vertical: false, brief: "b" });
  });

  it("never overwrites a file and makes safe names", () => {
    const taken = new Set(["/d/a.png", "/d/a-2.png"]);
    expect(uniquePath("/d", "a.png", (p) => taken.has(p))).toBe("/d/a-3.png");
    expect(slug("Hello, World! ünïcode")).toBe("hello-world-unicode");
    expect(slug("!!!")).toBe("c0mpute");
  });
});

describe("api helpers", () => {
  it("parses server-sent events across chunk boundaries", () => {
    const got: string[] = [];
    const feed = sseParser((d) => {
      got.push(d);
      return d === "[DONE]";
    });
    expect(feed('data: {"a":1}\n\nda')).toBe(false);
    expect(feed('ta: {"b":2}\n')).toBe(false);
    expect(feed("data: [DONE]\n")).toBe(true);
    expect(feed("data: after\n")).toBe(true);
    expect(got).toEqual(['{"a":1}', '{"b":2}', "[DONE]"]);
  });

  it("reads every error shape the API uses", () => {
    expect(errorMessage({ error: { message: "nope" } }, 400)).toBe("nope");
    expect(errorMessage({ error: "plain" }, 400)).toBe("plain");
    expect(errorMessage(null, 503)).toBe("HTTP 503");
  });
});

describe("config", () => {
  it("finds the key like the CLI: env, then chat.json", () => {
    const home = mkdtempSync(join(tmpdir(), "c0tui-"));
    expect(loadCreds({}, home)).toEqual({ url: "https://c0mpute.com", key: null, source: null });
    const path = saveCreds("https://c0mpute.com", "c0k_abc", home);
    const fd = openSync(path, "r");
    try {
      expect(fstatSync(fd).mode & 0o777).toBe(0o600);
      expect(JSON.parse(readFileSync(fd, "utf8"))).toEqual({ url: null, key: "c0k_abc" });
    } finally {
      closeSync(fd);
    }
    expect(loadCreds({}, home)).toEqual({ url: "https://c0mpute.com", key: "c0k_abc", source: "file" });
    expect(loadCreds({ C0MPUTE_API_KEY: "c0k_env", C0MPUTE_URL: "http://localhost:3000/" }, home)).toEqual({ url: "http://localhost:3000", key: "c0k_env", source: "env" });
  });

  it("defaults settings and reads them back", () => {
    const home = mkdtempSync(join(tmpdir(), "c0tui-"));
    const s = loadSettings({}, home);
    expect(s).toEqual({ defaultModel: null, downloadsDir: join(home, "Downloads", "c0mpute"), adultMode: false, preview: "auto", pushNode: null });
    mkdirSync(join(home, ".c0mpute"), { recursive: true });
    writeFileSync(join(home, ".c0mpute", "tui.json"), JSON.stringify({ defaultModel: "qwen3:4b", adultMode: true, preview: "weird" }));
    expect(loadSettings({}, home)).toMatchObject({ defaultModel: "qwen3:4b", adultMode: true, preview: "auto" });
  });
});

describe("node probes", () => {
  it("reads worker flags like the Rust CLI", () => {
    expect(workerFlags(["c0mpute", "worker", "status"])).toBeNull();
    expect(workerFlags(["/root/.c0mpute/bin/c0mpute", "worker", "start"])).toEqual({ roles: null, noStorage: false, noHosting: false, gpu: false, private: false });
    expect(workerFlags(["c0mpute", "worker", "start", "--roles", "tts,stt", "--no-storage", "--private"])).toMatchObject({ roles: ["tts", "stt"], noStorage: true, private: true });
    expect(workerFlags(["c0mpute", "worker", "start", "--roles=music"])?.roles).toEqual(["music"]);
  });

  it("parses /proc stat lines, even with spaces in the name", () => {
    const stat = "1234 (c0mpute worker) S 1 1234 1234 0 -1 4194560 0 0 0 0 10 5 0 0 20 0 30 0 987654 0 0";
    expect(parseStat(stat)).toEqual({ ppid: 1, startTicks: 987654 });
    expect(parseStat("garbage")).toBeNull();
  });

  it("names what a worker child is doing", () => {
    expect(classifyProcess(["/data/diffusion/bin/sd-cli", "-m", "z-image"])).toBe("image (Z-Image-Turbo)");
    expect(classifyProcess(["/data/diffusion/bin/sd-cli", "--model", "wan2.1"])).toBe("video (CPU, Wan 2.1)");
    expect(classifyProcess(["/data/music/venv/bin/python", "/data/music/music_worker.py"])).toBe("music (ACE-Step)");
    expect(classifyProcess(["ffmpeg", "-i", "x"])).toBe("ffmpeg (transcode / mux)");
    expect(classifyProcess(["/usr/bin/infernet", "daemon"])).toBeNull();
    expect(classifyProcess(["/bin/sh", "-c", "true"])).toBeNull();
  });

  it("finds the worker by PID file, systemd unit or process, and its jobs", () => {
    const procs: ProcInfo[] = [
      { pid: 100, ppid: 1, argv: ["/root/.c0mpute/bin/c0mpute (deleted)", "worker", "start"], ageS: 7200, uid: 0 },
      { pid: 101, ppid: 100, argv: ["/data/diffusion/bin/sd-cli", "-p", "x"], ageS: 30, uid: 0 },
      { pid: 102, ppid: 101, argv: ["ffmpeg", "-y"], ageS: 5, uid: 0 },
      { pid: 103, ppid: 100, argv: ["/usr/bin/infernet", "daemon"], ageS: 7000, uid: 0 },
      { pid: 200, ppid: 1, argv: ["/usr/bin/c0mpute", "worker", "status"], ageS: 1, uid: 1000 },
    ];
    const env = { platform: "linux" as const, home: mkdtempSync(join(tmpdir(), "c0tui-")) };
    const { worker, running } = probeWorker(env, procs, 1000, () => "c0mpute-worker.service");
    expect(worker).toMatchObject({ state: "running", pid: 100, via: "systemd", otherUser: true, ageS: 7200 });
    expect(running.map((r) => r.what)).toEqual(["image (Z-Image-Turbo)", "ffmpeg (transcode / mux)"]);
    expect(descendants(procs, 100).map((p) => p.pid)).toEqual([101, 102, 103]);
    // A PID file pointing at nothing: stale.
    mkdirSync(dataDir(env), { recursive: true });
    writeFileSync(join(dataDir(env), "worker.pid"), "999\n");
    expect(probeWorker(env, [], 1000).worker).toMatchObject({ state: "stale", pid: 999 });
    expect(probeWorker(env, procs.slice(1), 1000).worker.state).toBe("stale");
  });

  it("keeps INFO and up from the worker log, without colours", () => {
    expect(parseLogLine("\x1b[2m2026-11-11T00:57:06.808Z\x1b[0m \x1b[32m INFO\x1b[0m c0mpute_core: tracing initialised")).toEqual({
      time: "00:57:06",
      level: "INFO",
      message: "c0mpute_core: tracing initialised",
    });
    const log = [
      "2026-11-11T00:57:06.8Z DEBUG put: stored chunk",
      "2026-11-11T00:57:07.8Z  INFO dispatch: won auction",
      "2026-11-11T00:57:08.8Z  WARN diffusion: load gate",
      "",
    ].join("\n");
    expect(tailLog(log, 5).map((l) => l.level)).toEqual(["INFO", "WARN"]);
    expect(tailLog(log, 1).map((l) => l.message)).toEqual(["diffusion: load gate"]);
  });

  it("compares versions and reads job history", () => {
    expect(parseVersion("c0mpute 0.3.32\n")).toBe("0.3.32");
    expect(compareVersions("0.3.9", "0.3.10")).toBe(-1);
    expect(compareVersions("0.4.0", "0.3.99")).toBe(1);
    expect(compareVersions("0.3.32", "0.3.32")).toBe(0);
    const dir = mkdtempSync(join(tmpdir(), "c0tui-"));
    writeFileSync(join(dir, "music.txt2music.ms"), "214000");
    writeFileSync(join(dir, "diffusion.txt2img.ms"), "90000\n");
    utimesSync(join(dir, "diffusion.txt2img.ms"), new Date(NOW), new Date(NOW));
    const t = readJobTimes(dir);
    expect(t.map((x) => x.workload).sort()).toEqual(["diffusion.txt2img", "music.txt2music"]);
    expect(t.find((x) => x.workload === "music.txt2music")!.avgMs).toBe(214000);
    expect(readJobTimes(join(dir, "missing"))).toEqual([]);
  });

  it("accepts only a real status report", () => {
    expect(parseStatusJson("{}")).toBeNull();
    expect(parseStatusJson("not json")).toBeNull();
    // The base report alone (the desktop app's, CLI 0.3.33) has no role states.
    expect(parseStatusJson(JSON.stringify({ version: "0.3.33", roles: ["tts"], worker: { running: true } }))).toBeNull();
    expect(parseStatusJson(JSON.stringify({ role_states: [], tags: [] }))).toEqual({ role_states: [], tags: [] } as never);
  });
});

describe("network helpers", () => {
  it("formats like the web market page", () => {
    expect(priceLabel(null)).toBe("-");
    expect(priceLabel({ mode: "fixed", min_usd: 0.01, fixed_usd: 0.3 })).toBe("fixed $0.30");
    expect(priceLabel({ mode: "dynamic", min_usd: 0.02 })).toBe("dyn · min $0.02");
    expect(usd("0.000420")).toBe("$0.000420");
    expect(usd("4.2")).toBe("$4.20");
    expect(roleCounts({ network: { workers_online: 2, workers_with_role: { tts: 1, storage: 2, apps: 1 } } })).toEqual([
      ["storage", 2],
      ["apps", 1],
      ["tts", 1],
    ]);
  });
});

describe("bench", () => {
  it("summarises each workload with its best sample and scaling efficiency", () => {
    const s = summarizeWorkloads(demoBench);
    expect(s.map((x) => x.name)).toEqual(["fib", "matmul", "hash"]);
    expect(s[0]!.best.threads).toBe(24);
    expect(s[0]!.efficiency).toBeCloseTo(16 / 24, 5);
  });

  it("formats rates like the CLI and rejects malformed reports", () => {
    expect(humanRate("hash", 7.4e9)).toBe("6.89 GiB/s");
    expect(humanRate("matmul", 2.2e10)).toBe("22.00 GFLOP/s");
    expect(humanRate("fib", 4e8)).toBe("400 Mvis/s");
    expect(parseBenchReport("not json")).toBeNull();
    expect(parseBenchReport(JSON.stringify(demoBench))?.score).toBe(9_871);
  });

  it("resolves the data dir the way the Rust node does", () => {
    expect(dataDir({ platform: "linux", home: "/home/op" })).toBe("/home/op/.local/share/c0mpute");
    expect(dataDir({ platform: "linux", home: "/home/op", xdgDataHome: "/data" })).toBe("/data/c0mpute");
    expect(dataDir({ platform: "darwin", home: "/Users/op" })).toBe("/Users/op/Library/Application Support/com.c0mpute.c0mpute");
  });
});
