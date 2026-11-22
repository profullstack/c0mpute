import { describe, expect, it } from "bun:test";
import { renderToText, type KeyEvent } from "@profullstack/hqtui";
import { Api, type FetchLike } from "../api.ts";
import { Controller } from "../controller.ts";
import { demoFleet, demoState, NOW } from "../demo.ts";
import { buildRequest, confirmText, fleetRow, nodeDetail, pickAll, readOutcome, sortFleet, targets, togglePick, type FleetNode } from "../fleet.ts";
import { handleKey } from "../keys.ts";
import type { AppState } from "../state.ts";
import { c0mputeTheme } from "../theme.ts";
import { renderApp } from "../views/app.ts";

const keyEvent = (key: string): KeyEvent => ({ type: "key", name: key, key, char: key.length === 1 ? key : undefined, ctrl: false, alt: false, shift: false, raw: key });
const press = (ctl: Controller, ...keys: string[]) => {
  for (const k of keys) handleKey(ctl, keyEvent(k));
};
const draw = (s: AppState, width = 120, height = 36) => renderToText(({ ui, theme }) => renderApp(ui, s, theme), { width, height, theme: c0mputeTheme });
const tick = () => new Promise((r) => setTimeout(r, 20));

// sorted: attic (b2c3d4e5, online), gpu-box (a1b2c3d4, online), c3d4e5f6 (offline)
const NODES = sortFleet(demoFleet);

function fakeApi(routes: Record<string, (init?: RequestInit) => Response | Promise<Response>>, key: string | null = "c0k_test") {
  const calls: { method: string; path: string; body: unknown }[] = [];
  const fetchImpl: FetchLike = async (url, init) => {
    const path = url.replace("https://c0mpute.test", "");
    const method = init?.method ?? "GET";
    calls.push({ method, path, body: init?.body ? JSON.parse(String(init.body)) : undefined });
    const h = routes[`${method} ${path}`];
    if (!h) return new Response(JSON.stringify({ error: { message: `no route ${path}` } }), { status: 404 });
    return h(init);
  };
  return { api: new Api({ url: "https://c0mpute.test", key, source: key ? "file" : null }, fetchImpl), calls };
}

const fleetRoute = () => new Response(JSON.stringify({ nodes: demoFleet, totals: {}, fleet_key: null, payouts_ready: true }));

function fleetController(routes: Record<string, (init?: RequestInit) => Response | Promise<Response>> = {}, key: string | null = "c0k_test") {
  const { api, calls } = fakeApi({ "GET /api/v1/fleet": fleetRoute, ...routes }, key);
  const s = demoState();
  const ctl = new Controller({ ...s, creds: api.creds, tab: "fleet", chat: { ...s.chat, typing: false } }, { api, invalidate: () => {}, persist: false, probes: false, now: () => NOW });
  return { ctl, calls, posts: () => calls.filter((c) => c.method === "POST") };
}

describe("fleet rendering", () => {
  it("lists nodes with state, tags, jobs and earnings, and the stage-2 detail", () => {
    const s = demoState();
    const text = draw({ ...s, tab: "fleet", fleet: { ...s.fleet, sel: 1 } }, 140, 40);
    expect(text).toContain("6 Fleet");
    expect(text).toContain("3 nodes · 2 online");
    expect(text).toContain("a1b2c3d4");
    expect(text).toContain("gpu-box");
    expect(text).toContain("paused");
    expect(text).toContain("gpu,eu");
    expect(text).toContain("$1.50/$31.20");
    expect(text).toContain("serves 2 model(s) (limited to: qwen2.5:7b, gemma3:4b)");
    expect(text).toContain("house on · uncensored off · hosting on · storage on");
    expect(text).toContain("disk 512G free of 2.0T");
  });

  it("explains a node that has not reported state yet", () => {
    const old = NODES.find((n) => n.node === "c3d4e5f6")!;
    const lines = nodeDetail(old).join("\n");
    expect(lines).toContain("○ offline");
    expect(lines).toContain("no state reported yet");
    const row = fleetRow(old, false);
    expect(row).toMatchObject({ dot: "○", state: "offline", version: "0.3.90", tags: "old" });
    const s = demoState();
    expect(draw({ ...s, tab: "fleet", fleet: { ...s.fleet, sel: 2 } }, 140, 40)).toContain("no state reported yet");
  });

  it("shows a failed update and the paused state from node_state", () => {
    const attic = NODES[0]!;
    expect(nodeDetail(attic).join("\n")).toContain("update to 0.3.106 failed: download failed");
    expect(fleetRow(attic, true)).toMatchObject({ pick: "■", state: "paused", paused: true });
  });

  it("says how to sign in when there is no key", () => {
    const s = demoState();
    const text = draw({ ...s, tab: "fleet", creds: { ...s.creds, key: null, source: null } });
    expect(text).toContain("Not signed in. L: sign in here");
    expect(text).toContain("c0mpute login");
  });

  it("fits 80x24", () => {
    expect(draw({ ...demoState(), tab: "fleet" }, 80, 24)).toContain("Fleet");
  });
});

describe("fleet selection", () => {
  it("toggles, selects all and clears", () => {
    expect(togglePick([], "a")).toEqual(["a"]);
    expect(togglePick(["a", "b"], "a")).toEqual(["b"]);
    expect(pickAll(NODES, [])).toEqual(NODES.map((n) => n.node));
    expect(pickAll(NODES, NODES.map((n) => n.node))).toEqual([]);
  });

  it("acts on the selection in list order, else the highlighted node", () => {
    expect(targets(NODES, 1, [])).toEqual(["a1b2c3d4"]);
    expect(targets(NODES, 0, ["c3d4e5f6", "b2c3d4e5"])).toEqual(["b2c3d4e5", "c3d4e5f6"]);
    expect(targets(NODES, 0, ["gone0000"])).toEqual(["b2c3d4e5"]);
    expect(targets([], 0, [])).toEqual([]);
  });

  it("moves with j/k and arrows, Space picks, a picks all", () => {
    const { ctl } = fleetController();
    press(ctl, "j");
    expect(ctl.state.fleet.sel).toBe(1);
    press(ctl, "down", "down");
    expect(ctl.state.fleet.sel).toBe(2);
    press(ctl, "k");
    press(ctl, "space");
    expect(ctl.state.fleet.picked).toEqual(["a1b2c3d4"]);
    press(ctl, "a");
    expect(ctl.state.fleet.picked.length).toBe(3);
    press(ctl, "a");
    expect(ctl.state.fleet.picked).toEqual([]);
  });
});

describe("fleet actions need a confirmation", () => {
  it("n, Esc and Enter send nothing", async () => {
    const { ctl, posts } = fleetController({ "POST /api/v1/fleet/actions": () => new Response("{}") });
    for (const no of ["n", "escape", "enter"]) {
      press(ctl, "p");
      expect(ctl.state.fleet.confirm?.text).toBe("Pause 1 node (b2c3d4e5)? y/N");
      press(ctl, no);
      expect(ctl.state.fleet.confirm).toBeNull();
    }
    press(ctl, "o");
    press(ctl, "escape");
    press(ctl, "t");
    expect(ctl.state.fleet.prompt).not.toBeNull();
    press(ctl, "x", "escape");
    expect(ctl.state.fleet.prompt).toBeNull();
    await tick();
    expect(posts()).toEqual([]);
  });

  it("pause on a selection: y sends the exact body, shows results and refreshes", async () => {
    const { ctl, posts, calls } = fleetController({
      "POST /api/v1/fleet/actions": () =>
        new Response(JSON.stringify({ results: [{ node: "b2c3d4e5", ok: true, detail: "queued" }, { node: "a1b2c3d4", ok: true, detail: "queued" }, { node: "c3d4e5f6", ok: false, detail: "offline" }], ok: 2, failed: 1 })),
    });
    press(ctl, "a", "p");
    expect(ctl.state.fleet.confirm?.text).toBe("Pause 3 nodes (b2c3d4e5, a1b2c3d4, c3d4e5f6)? y/N");
    expect(posts()).toEqual([]);
    press(ctl, "y");
    await tick();
    expect(posts().map((c) => c.body)).toEqual([{ action: "pause", nodes: ["b2c3d4e5", "a1b2c3d4", "c3d4e5f6"], args: {} }]);
    expect(ctl.state.fleet.outcome).toMatchObject({ ok: 2, failed: 1, refused: false, nodeSide: true });
    expect(calls.at(-1)).toMatchObject({ method: "GET", path: "/api/v1/fleet" });
    const text = draw(ctl.state, 140, 40);
    expect(text).toContain("pause: 2 ok, 1 failed · node-side actions run on each node's next poll (about a minute)");
    expect(text).toContain("✗ c3d4e5f6  offline");
  });

  it("a toggle flips the highlighted node's value, can be switched, and sends {on}", async () => {
    const { ctl, posts } = fleetController({ "POST /api/v1/fleet/actions": () => new Response(JSON.stringify({ results: [{ node: "a1b2c3d4", ok: true, detail: "queued" }], ok: 1, failed: 0 })) });
    press(ctl, "j"); // gpu-box: hosting is on
    press(ctl, "o");
    expect(ctl.state.fleet.confirm?.text).toBe("Turn hosting OFF on 1 node (a1b2c3d4)? y/N");
    press(ctl, "t");
    expect(ctl.state.fleet.confirm?.text).toBe("Turn hosting ON on 1 node (a1b2c3d4)? y/N");
    press(ctl, "t", "y");
    await tick();
    expect(posts()[0]!.body).toEqual({ action: "hosting", nodes: ["a1b2c3d4"], args: { on: false } });
    expect(buildRequest("uncensored", ["x"], true)).toEqual({ action: "uncensored", nodes: ["x"], args: { on: true } });
    expect(buildRequest("house-models", ["x"], false)).toEqual({ action: "house-models", nodes: ["x"], args: { on: false } });
  });

  it("models: a comma separated prompt, then confirmation, then {models: [...]}", async () => {
    const { ctl, posts } = fleetController({ "POST /api/v1/fleet/actions": () => new Response(JSON.stringify({ results: [], ok: 0, failed: 0 })) });
    press(ctl, "space", "j", "space"); // attic + gpu-box
    press(ctl, "m");
    // Prefilled from the highlighted node's settings; clear it and type.
    expect(ctl.state.fleet.prompt?.input.value).toBe("qwen2.5:7b, gemma3:4b");
    ctl.setFleetInput({ value: "", cursor: 0 });
    for (const ch of "a, b ,") press(ctl, ch);
    press(ctl, "enter");
    expect(posts()).toEqual([]);
    expect(ctl.state.fleet.confirm?.text).toBe("Serve only a, b on 2 nodes (b2c3d4e5, a1b2c3d4)? y/N");
    press(ctl, "y");
    await tick();
    expect(posts()[0]!.body).toEqual({ action: "models", nodes: ["b2c3d4e5", "a1b2c3d4"], args: { models: ["a", "b"] } });
    expect(confirmText(buildRequest("models", ["x"], []))).toBe("Serve every model each node has on 1 node (x)? y/N");
  });

  it("tags go as a comma string; empty tags send nothing", async () => {
    const { ctl, posts } = fleetController({ "POST /api/v1/fleet/actions": () => new Response(JSON.stringify({ results: [{ node: "b2c3d4e5", ok: true, detail: "" }], ok: 1, failed: 0 })) });
    press(ctl, "t", "enter");
    expect(ctl.state.fleet.confirm).toBeNull();
    press(ctl, "T");
    for (const ch of "gpu, eu") press(ctl, ch);
    press(ctl, "enter");
    expect(ctl.state.fleet.confirm?.text).toBe('Remove tags "gpu,eu" from 1 node (b2c3d4e5)? y/N');
    press(ctl, "Y");
    await tick();
    expect(posts().map((c) => c.body)).toEqual([{ action: "untag", nodes: ["b2c3d4e5"], args: { tags: "gpu,eu" } }]);
    expect(ctl.state.fleet.outcome?.nodeSide).toBe(false);
  });

  it("a 409 shows each node's refusal, not a generic error", async () => {
    const { ctl } = fleetController({
      "POST /api/v1/fleet/actions": () => new Response(JSON.stringify({ results: [{ node: "c3d4e5f6", ok: false, detail: "node is too old for update" }], ok: 0, failed: 1 }), { status: 409 }),
    });
    press(ctl, "end", "U");
    expect(ctl.state.fleet.confirm?.text).toBe("Update 1 node (c3d4e5f6) to the latest c0mpute? y/N");
    press(ctl, "y");
    await tick();
    expect(ctl.state.fleet.outcome).toMatchObject({ refused: true, ok: 0, failed: 1 });
    const text = draw(ctl.state, 140, 40);
    expect(text).toContain("update: every node refused");
    expect(text).toContain("✗ c3d4e5f6  node is too old for update");
  });

  it("other errors become a toast with the server's message", () => {
    const req = buildRequest("restart", ["a"]);
    expect(() => readOutcome(req, 500, { error: "boom" })).toThrow("boom");
    expect(() => readOutcome(req, 409, { error: "nope" })).toThrow("nope");
  });
});

describe("fleet refresh", () => {
  it("loads on entering the tab and keeps the highlight on the same node", async () => {
    const { api, calls } = fakeApi({ "GET /api/v1/fleet": fleetRoute });
    const s = demoState();
    const ctl = new Controller({ ...s, creds: api.creds, tab: "settings", fleet: { ...s.fleet, sel: 1 } }, { api, invalidate: () => {}, persist: false, probes: false, now: () => NOW });
    ctl.setTab("fleet");
    await tick();
    expect(calls.map((c) => `${c.method} ${c.path}`)).toEqual(["GET /api/v1/fleet"]);
    expect(ctl.state.fleet.list.data!.nodes.map((n: FleetNode) => n.node)).toEqual(["b2c3d4e5", "a1b2c3d4", "c3d4e5f6"]);
    expect(ctl.state.fleet.sel).toBe(1);
  });

  it("signed out: no request, the login hint", async () => {
    const { ctl, calls } = fleetController({}, null);
    await ctl.refreshFleet();
    expect(calls).toEqual([]);
    expect(ctl.state.fleet.list.error).toContain("c0mpute chat login");
  });
});
