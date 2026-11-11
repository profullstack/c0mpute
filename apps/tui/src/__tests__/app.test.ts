import { describe, expect, it } from "bun:test";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { renderToText, type KeyEvent } from "@profullstack/hqtui";
import { Api, type FetchLike } from "../api.ts";
import { Controller } from "../controller.ts";
import { demoState, NOW } from "../demo.ts";
import { JobRunner, type Job } from "../jobs.ts";
import { handleKey } from "../keys.ts";
import { initialState, type AppState, type Tab } from "../state.ts";
import { c0mputeTheme } from "../theme.ts";
import { renderApp } from "../views/app.ts";
import { roleLines } from "../views/node.ts";
import { settingsItems } from "../views/settings.ts";

/** A key as the parser delivers it: printable keys carry `char`. */
const keyEvent = (key: string): KeyEvent => ({ type: "key", name: key, key, char: key.length === 1 ? key : undefined, ctrl: false, alt: false, shift: false, raw: key });

const draw = (s: AppState, width = 120, height = 36) => renderToText(({ ui, theme }) => renderApp(ui, s, theme), { width, height, theme: c0mputeTheme });
const tab = (t: Tab, over: Partial<AppState> = {}) => ({ ...demoState(), tab: t, ...over });

describe("every tab renders, at 80x24 and large", () => {
  for (const t of ["chat", "jobs", "node", "network", "settings"] as Tab[]) {
    it(`${t} fits 80x24 and 160x48`, () => {
      const small = draw(tab(t), 80, 24);
      expect(small).toContain("1 Chat");
      expect(small).toContain("5 Settings");
      expect(small).toContain("help");
      expect(draw(tab(t), 160, 48)).toContain("q quit");
    });
  }
});

describe("chat", () => {
  it("shows answers, a finished image with its file, and a song in progress", () => {
    const text = draw(tab("chat"));
    expect(text).toContain("you › what is a LoRA adapter?");
    expect(text).toContain("c0 › A LoRA adapter is a small set of low-rank");
    expect(text).toContain("saved image: ");
    expect(text).toContain("a-red-bicycle-leaning-on-a-blue-wall-f1e2d3c4.png");
    expect(text).toContain("song: Rain on the Window");
    expect(text).toContain("rendering on node …4QxW9tBk");
    expect(text).toMatch(/█+░+ 40%/);
    expect(text).toContain("anthony · balance $4.21");
  });

  it("says how to sign in when there is no key", () => {
    const s = demoState();
    const text = draw({ ...s, creds: { ...s.creds, key: null, source: null }, me: { data: null, error: null, at: null, loading: false }, chat: { ...s.chat, convo: { ...s.chat.convo, messages: [] } } });
    expect(text).toContain("not signed in · c0mpute chat login");
    expect(text).toContain("Not signed in: run `c0mpute chat login`");
  });
});

describe("jobs", () => {
  it("lists jobs with progress and details the selected one", () => {
    const text = draw(tab("jobs"));
    expect(text).toContain("2 running · 3 total");
    expect(text).toContain("Rain on the Window");
    expect(text).toContain("40%");
    expect(text).toContain("a paper boat drifting");
    expect(text).toContain("asked: lo-fi hip hop for a rainy night");
    expect(text).toContain("worker …4QxW9tBk");
    expect(text).toContain("x stop watching");
    expect(text).toContain("The API has no cancel");
    const video = draw(tab("jobs", { jobSel: 1 }));
    expect(video).toContain("queued for a c0mpute video worker (#2 in");
  });
});

describe("node", () => {
  it("shows the worker, every role, gated ones with the reason, and tags", () => {
    const text = draw(tab("node"), 140, 40);
    expect(text).toContain("0.3.33 · up to date · auto-update on");
    expect(text).toContain("running · pid 2442803 · systemd");
    expect(text).toContain("6/9 roles serving");
    expect(text).toContain("✗ diffusion load gate: load 35.2 on 16 CPUs");
    expect(text).toContain("✓ music     ace-step-1.5");
    expect(text).toContain("tesseract is not installed");
    expect(text).toContain("advertised: c0mpute:e2e:v1");
    expect(text).toContain("music (ACE-Step) 1m12s");
    expect(text).toContain("won auction workload=music.txt2music");
  });

  it("explains an old CLI and a stopped worker", () => {
    const s = demoState();
    const node = { ...s.node, status: null, statusText: ["version: c0mpute 0.3.20", "worker: not running"], statusError: "this c0mpute is older than the TUI: `c0mpute update` adds per-role gates", worker: { ...s.node.worker, state: "stopped" as const, pid: null }, cliVersion: "0.3.20" };
    const text = draw({ ...s, tab: "node", node }, 120, 36);
    expect(text).toContain("0.3.20 · 0.3.33 available: c0mpute update");
    expect(text).toContain("not running: c0mpute worker start -d");
    expect(text).toContain("this c0mpute is older than the TUI");
    expect(text).toContain("worker: not running");
  });

  it("warns when the worker runs as another user", () => {
    const s = demoState();
    const lines = roleLines({ ...s.node, worker: { ...s.node.worker, otherUser: true } }, 100, c0mputeTheme).map((l) => l.text);
    expect(lines.join("\n")).toContain("worker runs as another user");
  });

  it("toggles to the bench panel", () => {
    expect(draw(tab("node", { nodePanel: "bench" }), 140, 40)).toContain("score 9,871");
  });
});

describe("network", () => {
  it("shows workers, role counts, nodes and prices", () => {
    const text = draw(tab("network"));
    expect(text).toContain("15 workers online");
    expect(text).toContain("tts 10");
    expect(text).toContain("XtPja8R9");
    expect(text).toContain("dyn · min $0.02");
    const prices = draw(tab("network", { netPanel: "prices" }));
    expect(prices).toContain("qwen2.5:7b");
    expect(prices).toContain("demand x1.25");
  });

  it("says offline instead of going blank", () => {
    const s = demoState();
    const err = { data: null, error: "offline: could not reach c0mpute.com (timed out)", at: null, loading: false };
    const text = draw({ ...s, tab: "network", net: { status: err, nodes: err, pricing: err } });
    expect(text).toContain("offline");
    expect(text).toContain("could not reach c0mpute.com");
  });
});

describe("settings", () => {
  it("offers the 18+ toggle exactly as the web does", () => {
    const s = demoState();
    const off = settingsItems(s).find((i) => i.row === "adult")!;
    expect(off.value).toBe("off (account not 18+ verified)");
    expect(off.hint).toContain("an API key cannot attest");
    const verified = { ...s, me: { ...s.me, data: { ...s.me.data!, adult_verified_at: "2026-11-10T00:00:00Z" } } };
    const on = settingsItems({ ...verified, settings: { ...s.settings, adultMode: true } }).find((i) => i.row === "adult")!;
    expect(on.value).toBe("on for this terminal");
    expect(on.hint).toContain("remove 18+ verification");
  });

  it("shows the sign-in flow", () => {
    const text = draw(tab("settings", { login: { link: "https://c0mpute.com/cli/approve?code=ABCD-EFGH", code: "ABCD-EFGH", status: "waiting for approval…", done: false } }));
    expect(text).toContain("Sign in to c0mpute.com");
    expect(text).toContain("ABCD-EFGH");
  });
});

describe("overlays", () => {
  it("help lists global keys and the slash commands", () => {
    const text = draw({ ...demoState(), help: true }, 120, 40);
    expect(text).toContain("next / previous tab");
    expect(text).toContain("/image <prompt>");
    expect(text).toContain("/push hosting");
  });

  it("the model picker lists the live models", () => {
    const text = draw({ ...demoState(), picker: { selected: 1, target: "chat" } });
    expect(text).toContain("any live model");
    expect(text).toContain("deepseek-coder-v2:16b");
  });
});

// ─── controller + keys against a fake c0mpute.com ─────────────────────────

function sse(chunks: string[]): Response {
  const body = new ReadableStream({
    start(c) {
      for (const ch of chunks) c.enqueue(new TextEncoder().encode(ch));
      c.close();
    },
  });
  return new Response(body, { status: 200, headers: { "content-type": "text/event-stream" } });
}

function fakeApi(routes: Record<string, (init?: RequestInit) => Response | Promise<Response>>, key: string | null = "c0k_test"): { api: Api; calls: string[] } {
  const calls: string[] = [];
  const fetchImpl: FetchLike = async (url, init) => {
    const path = url.replace("https://c0mpute.test", "");
    calls.push(`${init?.method ?? "GET"} ${path}`);
    const h = routes[`${init?.method ?? "GET"} ${path}`] ?? routes[path];
    if (!h) return new Response(JSON.stringify({ error: { message: `no route ${path}` } }), { status: 404 });
    return h(init);
  };
  return { api: new Api({ url: "https://c0mpute.test", key, source: key ? "file" : null }, fetchImpl), calls };
}

function controller(api: Api, over: Partial<AppState> = {}): Controller {
  const base = initialState({
    creds: api.creds,
    settings: { defaultModel: null, downloadsDir: mkdtempSync(join(tmpdir(), "c0dl-")), adultMode: false, preview: "auto", pushNode: null },
    bench: { bench: null, benchPath: "/x", benchError: null },
    now: NOW,
  });
  return new Controller({ ...base, ...over }, { api, invalidate: () => {}, persist: false, probes: false });
}

const type = (ctl: Controller, text: string) => {
  for (const ch of text) handleKey(ctl, keyEvent(ch));
};

describe("controller", () => {
  it("streams an answer into the conversation", async () => {
    let body = "";
    const { api } = fakeApi({
      "POST /api/v1/chat/completions": (init) => {
        body = String(init?.body);
        return sse(['data: {"choices":[{"delta":{"content":"Hel"}}]}\n', 'data: {"choices":[{"delta":{"content":"lo"}}]}\n\ndata: [DONE]\n']);
      },
    });
    const ctl = controller(api);
    type(ctl, "hi there");
    handleKey(ctl, keyEvent("enter"));
    await new Promise((r) => setTimeout(r, 20));
    const msgs = ctl.state.chat.convo.messages;
    expect(msgs.map((m) => [m.role, m.content])).toEqual([
      ["user", "hi there"],
      ["assistant", "Hello"],
    ]);
    expect(JSON.parse(body)).toEqual({ messages: [{ role: "user", content: "hi there" }], stream: true });
    expect(ctl.state.chat.convo.title).toBe("hi there");
    expect(ctl.state.chat.streaming).toBe(false);
  });

  it("queues a line typed during an answer and sends it when the answer finishes", async () => {
    const bodies: string[] = [];
    let finish: () => void = () => {};
    const { api } = fakeApi({
      "POST /api/v1/chat/completions": (init) => {
        bodies.push(String(init?.body));
        if (bodies.length > 1) return sse(['data: {"choices":[{"delta":{"content":"second"}}]}\n\ndata: [DONE]\n']);
        const body = new ReadableStream({
          start(c) {
            c.enqueue(new TextEncoder().encode('data: {"choices":[{"delta":{"content":"first"}}]}\n'));
            finish = () => {
              c.enqueue(new TextEncoder().encode("data: [DONE]\n"));
              c.close();
            };
          },
        });
        return new Response(body, { status: 200, headers: { "content-type": "text/event-stream" } });
      },
    });
    const ctl = controller(api);
    type(ctl, "one");
    handleKey(ctl, keyEvent("enter"));
    await new Promise((r) => setTimeout(r, 10));
    expect(ctl.state.chat.streaming).toBe(true);
    type(ctl, "two");
    handleKey(ctl, keyEvent("enter"));
    expect(ctl.state.chat.queued).toEqual(["two"]);
    expect(ctl.state.chat.input.value).toBe("");
    expect(draw({ ...ctl.state, tab: "chat" })).toContain("queued › two");
    expect(bodies.length).toBe(1);
    finish();
    await new Promise((r) => setTimeout(r, 20));
    expect(ctl.state.chat.queued).toEqual([]);
    expect(ctl.state.chat.convo.messages.map((m) => [m.role, m.content])).toEqual([
      ["user", "one"],
      ["assistant", "first"],
      ["user", "two"],
      ["assistant", "second"],
    ]);
    expect(JSON.parse(bodies[1]!).messages.at(-1)).toEqual({ role: "user", content: "two" });
  });

  it("Esc stops the answer and the queued line goes at once; up takes a queued line back", async () => {
    const bodies: string[] = [];
    const { api } = fakeApi({
      "POST /api/v1/chat/completions": (init) => {
        bodies.push(String(init?.body));
        if (bodies.length > 1) return sse(['data: {"choices":[{"delta":{"content":"ok"}}]}\n\ndata: [DONE]\n']);
        const signal = init?.signal;
        const body = new ReadableStream({
          start(c) {
            c.enqueue(new TextEncoder().encode('data: {"choices":[{"delta":{"content":"long"}}]}\n'));
            signal?.addEventListener("abort", () => c.error(new DOMException("aborted", "AbortError")));
          },
        });
        return new Response(body, { status: 200, headers: { "content-type": "text/event-stream" } });
      },
    });
    const ctl = controller(api);
    type(ctl, "one");
    handleKey(ctl, keyEvent("enter"));
    await new Promise((r) => setTimeout(r, 10));
    type(ctl, "draft");
    handleKey(ctl, keyEvent("enter"));
    type(ctl, "two");
    handleKey(ctl, keyEvent("enter"));
    handleKey(ctl, keyEvent("up"));
    expect(ctl.state.chat.input.value).toBe("two");
    expect(ctl.state.chat.queued).toEqual(["draft"]);
    for (let i = 0; i < 3; i++) handleKey(ctl, keyEvent("backspace"));
    handleKey(ctl, keyEvent("escape"));
    await new Promise((r) => setTimeout(r, 20));
    expect(bodies.length).toBe(2);
    expect(JSON.parse(bodies[1]!).messages.at(-1)).toEqual({ role: "user", content: "draft" });
    expect(ctl.state.chat.convo.messages.at(-1)!.content).toBe("ok");
  });

  it("tells a signed-out user the exact command, without calling the API", async () => {
    const { api, calls } = fakeApi({}, null);
    const ctl = controller(api);
    await ctl.run({ kind: "image", prompt: "a cat" }, "/image a cat");
    expect(calls).toEqual([]);
    expect(ctl.state.chat.convo.messages.at(-1)!.content).toContain("c0mpute chat login");
  });

  it("runs an image job from the chat to a saved file", async () => {
    let polls = 0;
    const png = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 1, 2, 3]);
    const { api, calls } = fakeApi({
      "POST /api/v1/images": () => new Response(JSON.stringify({ id: "11111111-2222-4333-8444-555555555555", poll: "/api/v1/images/11111111-2222-4333-8444-555555555555" }), { status: 202 }),
      "GET /api/v1/images/11111111-2222-4333-8444-555555555555": () =>
        ++polls === 1 ? new Response(JSON.stringify({ stage: "queued", queue_position: 1 }), { status: 202 }) : new Response(png, { status: 200, headers: { "content-type": "image/png" } }),
    });
    const ctl = controller(api);
    const runner = new JobRunner({ api, downloadsDir: () => ctl.state.settings.downloadsDir, onChange: () => {} });
    void runner;
    await ctl.run({ kind: "image", prompt: "a red bicycle" }, "/image a red bicycle");
    const turn = ctl.state.chat.convo.messages.at(-1)!;
    expect(turn.jobId).toBe("11111111-2222-4333-8444-555555555555");
    expect(calls[0]).toBe("POST /api/v1/images");
    const deadline = Date.now() + 15_000;
    let job: Job | undefined;
    while (Date.now() < deadline) {
      job = ctl.state.jobs[0];
      if (job?.status === "done" || job?.status === "failed") break;
      await new Promise((r) => setTimeout(r, 100));
    }
    expect(job?.status).toBe("done");
    expect(job!.files[0]!.path).toEndWith("a-red-bicycle-11111111.png");
    expect(Array.from(readFileSync(job!.files[0]!.path))).toEqual(Array.from(png));
    ctl.stop();
  }, 20_000);

  it("Esc leaves the input; then digits switch tabs and q quits", () => {
    const { api } = fakeApi({});
    const ctl = controller(api);
    type(ctl, "q2");
    expect(ctl.state.chat.input.value).toBe("q2");
    expect(handleKey(ctl, keyEvent("escape"))).toBeUndefined();
    expect(ctl.state.chat.typing).toBe(false);
    handleKey(ctl, keyEvent("3"));
    expect(ctl.state.tab).toBe("node");
    handleKey(ctl, keyEvent("tab"));
    expect(ctl.state.tab).toBe("network");
    handleKey(ctl, keyEvent("?"));
    expect(ctl.state.help).toBe(true);
    handleKey(ctl, keyEvent("escape"));
    expect(ctl.state.help).toBe(false);
    expect(handleKey(ctl, keyEvent("q"))).toBe("quit");
  });

  it("will not turn 18+ on for an account that has not attested", () => {
    const { api } = fakeApi({});
    const ctl = controller(api);
    ctl.toggleAdult();
    expect(ctl.state.settings.adultMode).toBe(false);
    expect(ctl.state.toast?.text).toContain("c0mpute.com/chat settings");
  });

  it("picks a default model from Settings and a chat model from Chat", () => {
    const { api } = fakeApi({});
    const ctl = controller(api, { models: { data: ["qwen3:4b", "qwen2.5:7b"], error: null, at: NOW, loading: false } });
    ctl.openPicker("default");
    ctl.movePicker(2);
    ctl.choosePicker();
    expect(ctl.state.settings.defaultModel).toBe("qwen2.5:7b");
    expect(ctl.state.chat.convo.model).toBeNull();
    ctl.openPicker();
    ctl.choosePicker(1);
    expect(ctl.state.chat.convo.model).toBe("qwen3:4b");
  });
});
