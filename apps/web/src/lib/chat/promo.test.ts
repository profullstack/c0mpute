import { describe, expect, test } from "bun:test";
import { describePromoProgress, detectPromoRequest, parsePromoCommand, parsePromoRequest, sanitizePromoProgress } from "./promo";
import { handleRpc, TOOLS } from "./mcp";

describe("chat promo", () => {
  test("parses /promo with style and vertical", () => {
    expect(parsePromoCommand("/promo https://c0mpute.com pixar style")).toMatchObject({ url: "https://c0mpute.com", style: "pixar", vertical: false });
    expect(parsePromoCommand("/promo c0mpute.com anime 9:16")).toMatchObject({ url: "https://c0mpute.com", style: "anime", vertical: true });
    expect(parsePromoCommand("/promo https://x.io")).toMatchObject({ style: "pixar" });
    expect(parsePromoCommand("/promo no link here")).toBeNull();
    expect(parsePromoCommand("/promotion https://x.io")).toBeNull();
  });

  test("routes plain-language promo asks with a link", () => {
    expect(detectPromoRequest("create a pixar style promo video for https://c0mpute.com")).toMatchObject({ url: "https://c0mpute.com", style: "pixar" });
    expect(detectPromoRequest("can you make a claymation cartoon demo video of infernetprotocol.com")).toMatchObject({ url: "https://infernetprotocol.com", style: "claymation" });
    expect(detectPromoRequest("summarize https://c0mpute.com")).toBeNull();
    expect(detectPromoRequest("/music a promo jingle")).toBeNull();
  });

  test("validates the request", () => {
    expect(parsePromoRequest({ url: "c0mpute.com" })).toEqual({ url: "https://c0mpute.com/", style: "pixar", vertical: false, brief: "" });
    expect(parsePromoRequest({ url: "https://a.com", style: "Stop-Motion", vertical: true })).toMatchObject({ style: "claymation", vertical: true });
    expect(parsePromoRequest({ url: "ftp://a.com" })).toBeTypeOf("string");
    expect(parsePromoRequest({ url: "http://localhost:3000" })).toBeTypeOf("string");
    expect(parsePromoRequest({ url: "https://u:p@a.com" })).toBeTypeOf("string");
    expect(parsePromoRequest({})).toBeTypeOf("string");
  });

  test("keeps only known progress fields", () => {
    const p = sanitizePromoProgress("id", {
      status: "pending",
      stages: { brand: { state: "done", detail: "c0mpute" }, frames: { state: "running", detail: "2/6" }, evil: { state: "done" }, voice: { state: "weird" } },
      scenes: [{ caption: "a", narration: "b", duration: 4, frame: true, clip: false, extra: "x" }],
      brand: { name: "c0mpute", colors: { primary: "#14b8a6", background: "red;}" } },
      dir: "/tmp/secret",
    });
    expect(Object.keys(p.stages)).toEqual(["brand", "frames"]);
    expect(p.scenes[0]).toEqual({ caption: "a", narration: "b", duration: 4, frame: true, clip: false });
    expect(p.brand?.colors).toEqual({ primary: "#14b8a6" });
    expect(JSON.stringify(p)).not.toContain("secret");
    expect(describePromoProgress(p).split("\n")).toHaveLength(7);
    expect(describePromoProgress(p)).toContain("[x] brand: c0mpute");
    expect(sanitizePromoProgress("id", { status: "failed", error: "boom" }).error).toBe("boom");
  });

  test("MCP: promo_video starts a job and promo_status reports it", async () => {
    expect(TOOLS.map((t) => t.name)).toContain("promo_video");
    const calls: string[] = [];
    const upstream = async (path: string, init?: RequestInit) => {
      calls.push(`${init?.method ?? "GET"} ${path}`);
      if (path === "/api/chat/promo") return new Response(JSON.stringify({ id: "11111111-1111-4111-8111-111111111111", poll: "/x" }), { status: 202 });
      return new Response(JSON.stringify({ status: "done", duration: 31.5, motion: "kenburns", stages: {}, scenes: [] }), { status: 200 });
    };
    const ctx = { user: { id: "u", did: "did:x", name: null }, upstream };
    const start = (await handleRpc({ id: 1, method: "tools/call", params: { name: "promo_video", arguments: { url: "https://c0mpute.com" } } }, ctx)) as { result: { content: { text: string }[] } };
    expect(JSON.parse(start.result.content[0].text).id).toBe("11111111-1111-4111-8111-111111111111");
    const status = (await handleRpc({ id: 2, method: "tools/call", params: { name: "promo_status", arguments: { id: "11111111-1111-4111-8111-111111111111" } } }, ctx)) as { result: { content: { text: string }[] } };
    const s = JSON.parse(status.result.content[0].text);
    expect(s.status).toBe("done");
    expect(s.video).toMatch(/\/api\/v1\/promo\/11111111-1111-4111-8111-111111111111\/video$/);
    expect(calls).toEqual(["POST /api/chat/promo", "GET /api/chat/promo/11111111-1111-4111-8111-111111111111"]);
  });
});
