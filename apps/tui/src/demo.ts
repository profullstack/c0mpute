/**
 * A fixed, realistic AppState for tests and `scripts/screenshot.ts --demo`:
 * one finished image, a song in progress, a queued clip, a node with some
 * roles serving and some gated, and a small network.
 */
import type { BenchReport } from "./bench.ts";
import type { Job } from "./jobs.ts";
import type { NodeStatusJson } from "./node.ts";
import { initialState, type AppState } from "./state.ts";

export const NOW = Date.UTC(2026, 10, 11, 12, 0, 0);

function sample(threads: number, durationUs: number, single: number, throughput: number) {
  return { params: "n=34", threads, result: { duration: `${durationUs} us`, duration_us: durationUs, scaled: 1, speedup: single / durationUs, throughput } };
}

export const demoBench: BenchReport = {
  metadata: { start_time: "2026-11-10T19:00:00Z", cpu: "AMD Ryzen 9 5900X 12-Core Processor", cores: 24, kernel: "Linux 7.0.0", compiler: "rustc 1.92.0", version: "0.3.32", quick: false, elapsed_ms: 8123 },
  results: {
    c0mpute: {
      fib: [sample(1, 4_000_000, 4_000_000, 4e8), sample(8, 600_000, 4_000_000, 2.6e9), sample(24, 250_000, 4_000_000, 6.4e9)],
      matmul: [sample(1, 130_000, 130_000, 2e9), sample(24, 12_000, 130_000, 2.2e10)],
      hash: [sample(1, 45_000, 45_000, 1.5e9), sample(24, 9_000, 45_000, 7.4e9)],
    },
  },
  score: 9_871,
};

const base = (over: Partial<Job>): Job => ({
  id: "00000000-0000-4000-8000-000000000000",
  kind: "image",
  prompt: "",
  request: {},
  poll: null,
  status: "pending",
  stage: "",
  lines: [],
  worker: null,
  progress: null,
  etaS: null,
  queuePosition: null,
  title: null,
  error: null,
  createdAt: NOW - 60_000,
  updatedAt: NOW,
  finishedAt: null,
  files: [],
  adult: false,
  lyrics: null,
  ...over,
});

export const demoJobs: Job[] = [
  base({
    id: "6b0f2a51-1f7e-4a8e-9a39-1d2b8e4c7a10",
    kind: "music",
    prompt: "lo-fi hip hop for a rainy night",
    request: { prompt: "lo-fi hip hop for a rainy night", lyrics: "" },
    poll: "/api/v1/music/6b0f2a51-1f7e-4a8e-9a39-1d2b8e4c7a10",
    status: "pending",
    title: "Rain on the Window",
    stage: "rendering on node …4QxW9tBk · 1:12 in · ~1:48 left (node's estimate)",
    worker: "12D3KooWDemoWorker4QxW9tBk",
    progress: 0.4,
    etaS: 108,
    createdAt: NOW - 150_000,
  }),
  base({
    id: "a3c9d1e2-5b6f-4c7d-8e9f-0a1b2c3d4e5f",
    kind: "video",
    prompt: "a paper boat drifting across a pond at dawn",
    request: { prompt: "a paper boat drifting across a pond at dawn", model: "wan2.1-t2v-1.3b" },
    poll: "/api/v1/videos/a3c9d1e2-5b6f-4c7d-8e9f-0a1b2c3d4e5f",
    status: "pending",
    stage: "queued for a c0mpute video worker (#2 in line); it starts when one is free",
    queuePosition: 2,
    createdAt: NOW - 420_000,
  }),
  base({
    id: "f1e2d3c4-b5a6-4789-9abc-def012345678",
    kind: "image",
    prompt: "a red bicycle leaning on a blue wall",
    request: { prompt: "a red bicycle leaning on a blue wall" },
    poll: "/api/v1/images/f1e2d3c4-b5a6-4789-9abc-def012345678",
    status: "done",
    stage: "done in 2:04",
    progress: 1,
    createdAt: NOW - 900_000,
    finishedAt: NOW - 776_000,
    files: [{ path: "/home/op/Downloads/c0mpute/a-red-bicycle-leaning-on-a-blue-wall-f1e2d3c4.png", type: "image/png", label: "image" }],
  }),
];

export const demoStatus: NodeStatusJson = {
  version: "0.3.33",
  config_path: "/home/op/.config/c0mpute/config.toml",
  peer_id: "12D3KooWPZE9n5VjvvudVXGdkoN7wZy9jJ7F8uTwETiWnvseXUug",
  name: "profullstack",
  private: false,
  auto_update: true,
  worker_process: { pid: 2442803, argv: ["/home/op/.c0mpute/bin/c0mpute", "worker", "start"] },
  roles_source: "worker defaults",
  hardware: "c0mpute:cpu",
  role_states: [
    { role: "storage", serving: true, reason: null, detail: null },
    { role: "tts", serving: true, reason: null, detail: "piper, kokoro" },
    { role: "stt", serving: true, reason: null, detail: "faster-whisper" },
    { role: "diffusion", serving: false, reason: "load gate: load 35.2 on 16 CPUs, takes work below 12.0", detail: null },
    { role: "music", serving: true, reason: null, detail: "ace-step-1.5" },
    { role: "video", serving: false, reason: "GPU: no NVIDIA GPU detected (nvidia-smi found none); CPU: CPU video not installed (`c0mpute video setup --cpu` opts in)", detail: null },
    { role: "ocr", serving: false, reason: "tesseract is not installed (apt install tesseract-ocr poppler-utils)", detail: null },
    { role: "inference", serving: true, reason: null, detail: "qwen2.5:7b, qwen3:4b" },
    { role: "hosting", serving: true, reason: null, detail: null },
  ],
  tags: ["c0mpute:e2e:v1", "c0mpute:role:storage", "c0mpute:role:tts", "c0mpute:role:stt", "c0mpute:role:music", "c0mpute:music:model:ace-step-1.5", "infernet:inference", "c0mpute:role:hosting", "c0mpute:cpu"],
  host: { cpus: 16, job_threads: 8, load_1m: 35.2, ram_total_bytes: 33.6e9, ram_free_bytes: 13.3e9 },
};

export function demoState(): AppState {
  const s = initialState({
    creds: { url: "https://c0mpute.com", key: "c0k_demo", source: "file" },
    settings: { defaultModel: null, downloadsDir: "/home/op/Downloads/c0mpute", adultMode: false, preview: "auto", pushNode: null },
    bench: { bench: demoBench, benchPath: "/home/op/.local/share/c0mpute/bench.json", benchError: null },
    jobs: demoJobs,
    now: NOW,
    convo: {
      id: "cdemo",
      title: "what is a LoRA adapter?",
      model: null,
      messages: [
        { role: "user", content: "what is a LoRA adapter?", at: NOW - 1_000_000 },
        {
          role: "assistant",
          content:
            "A LoRA adapter is a small set of low-rank weight matrices trained on top of a frozen base model. Instead of fine-tuning every weight, you learn two thin matrices per layer whose product is added to the original weights, so the adapter is a few MB and can be swapped in and out at load time.",
          at: NOW - 990_000,
        },
        { role: "user", content: "/image a red bicycle leaning on a blue wall", at: NOW - 900_000 },
        { role: "assistant", content: "", jobId: demoJobs[2]!.id, at: NOW - 900_000 },
        { role: "user", content: "/music lo-fi hip hop for a rainy night", at: NOW - 150_000 },
        { role: "assistant", content: "", jobId: demoJobs[0]!.id, at: NOW - 150_000 },
      ],
    },
  });
  return {
    ...s,
    me: {
      data: { id: "u1", did: "did:key:z6MkjzDfDemoDemoDemoDemoDemoDemo", name: "anthony", adult_verified_at: null, balance: { spendable_usd: "4.210000", earned_spendable_usd: "1.034200", prepaid_available_usd: "3.175800" } },
      error: null,
      at: NOW,
      loading: false,
    },
    models: { data: ["qwen2.5:7b", "qwen3:4b", "deepseek-coder-v2:16b", "gemma3:4b"], error: null, at: NOW, loading: false },
    node: {
      cliPath: "/home/op/.c0mpute/bin/c0mpute",
      cliVersion: "0.3.33",
      latest: "0.3.33",
      worker: { state: "running", pid: 2442803, via: "systemd", otherUser: false, ageS: 3 * 86400 + 4000, flags: { roles: null, noStorage: false, noHosting: false, gpu: false, private: false } },
      status: demoStatus,
      statusText: null,
      statusError: null,
      statusAt: NOW - 20_000,
      running: [{ pid: 2450001, what: "music (ACE-Step)", ageS: 72 }],
      jobTimes: [{ workload: "music.txt2music", avgMs: 214_000, lastAt: NOW - 3_600_000 }],
      logs: [
        { time: "11:58:02", level: "INFO", message: "c0mpute_core::dispatch: won auction workload=music.txt2music price_usd=0.004" },
        { time: "11:58:03", level: "INFO", message: "c0mpute_core::music: rendering song duration_s=120" },
        { time: "11:59:40", level: "WARN", message: "c0mpute_core::diffusion: load gate: not bidding on diffusion.txt2img" },
      ],
      logSource: "journalctl -u c0mpute-worker",
    },
    net: {
      status: {
        data: {
          network: {
            workers_online: 15,
            workers_with_role: { storage: 9, tts: 10, stt: 10, "workflow-step": 10, crawl: 8, gateway: 8, hosting: 7, embed: 5, captcha: 5, transcode: 4, diffusion: 2, music: 2, video: 1, render: 1, ocr: 1 },
            jobs_in_flight: 3,
            jobs_completed_24h: 412,
          },
        },
        error: null,
        at: NOW,
        loading: false,
      },
      nodes: {
        data: {
          generated_at: "2026-11-11T12:00:00Z",
          workers_online: 3,
          nodes: [
            { id: "XtPja8R9", class: "cpu", cores: 32, ram_bytes: 97.9e9, bench_score: 18934, version: "0.3.32", name: "profullstack", pricing: { mode: "dynamic", min_usd: 0 }, last_seen_s: 12 },
            { id: "AmaKBdpB", class: "cpu", cores: 16, ram_bytes: 33.6e9, bench_score: 9871, version: "0.3.32", name: "profullstack", pricing: { mode: "dynamic", min_usd: 0.02 }, last_seen_s: 40 },
            { id: "vxhypiW8", class: "gpu", cores: 8, ram_bytes: 16e9, bench_score: 6120, version: "0.3.30", name: null, pricing: { mode: "fixed", min_usd: 0.01, fixed_usd: 0.3 }, last_seen_s: 75 },
          ],
        },
        error: null,
        at: NOW,
        loading: false,
      },
      pricing: {
        data: {
          models: [
            { model: "qwen2.5:7b", input_per_m_usd: 0.05, output_per_m_usd: 0.2, live: true, market: { multiplier: 1.25, reason: "demand", providers: 3 } },
            { model: "gemma3:4b", input_per_m_usd: 0.035, output_per_m_usd: 0.14, live: false, market: { multiplier: 1, reason: "no_signal", providers: 0 } },
          ],
        },
        error: null,
        at: NOW,
        loading: false,
      },
    },
  };
}
