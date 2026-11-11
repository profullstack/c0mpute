import { describe, expect, test } from "bun:test";
import { gateGeneration } from "./adult";
import { describeVideoProgress, MAX_WAIT_S, parseVideoCommand, parseVideoRequest, preferWait, videoPollOutcome, videoRequestBody } from "./videos";

describe("videos", () => {
  test("defaults to Wan 2.1 1.3B, text only", () => {
    expect(parseVideoRequest({ prompt: " a lighthouse at dusk " })).toEqual({ prompt: "a lighthouse at dusk", model: "wan2.1-t2v-1.3b" });
    expect(typeof parseVideoRequest({ prompt: "x", model: "sora" })).toBe("string");
    // Adult clips on the default model go to the GPU model; an explicit 5B stays; the env override wins.
    expect(parseVideoRequest({ prompt: "x", adult: true })).toEqual({ prompt: "x", model: "wan2.2-ti2v-5b", adult: true });
    expect(parseVideoRequest({ prompt: "x", adult: true, model: "wan2.2-ti2v-5b" })).toEqual({ prompt: "x", model: "wan2.2-ti2v-5b", adult: true });
    expect(parseVideoRequest({ prompt: "x", adult: true }, { adultModel: "wan2.1-t2v-1.3b" })).toEqual({ prompt: "x", model: "wan2.1-t2v-1.3b", adult: true });
    expect(parseVideoRequest({ prompt: "x", adult: true }, { adultModel: "bogus" })).toEqual({ prompt: "x", model: "wan2.2-ti2v-5b", adult: true });
    expect(typeof parseVideoRequest({ prompt: "x", image_base64: "AAAA" })).toBe("string");
    expect(typeof parseVideoRequest({ prompt: "" })).toBe("string");
    expect(parseVideoRequest({ prompt: "x", adult: true })).toMatchObject({ adult: true });
    expect(parseVideoRequest({ prompt: "x", adult: 1 })).not.toHaveProperty("adult");
  });

  test("Prefer: wait=N is clamped to 2 h and to the proxy cap", () => {
    expect(preferWait(null)).toBe(0);
    expect(preferWait("respond-async")).toBe(0);
    expect(preferWait("wait=30")).toBe(30);
    expect(preferWait("respond-async, wait=600")).toBe(600);
    expect(preferWait("wait=99999")).toBe(MAX_WAIT_S);
    expect(preferWait("wait=3600", 280)).toBe(280);
  });

  test("video requests pass the same content policy, including the negative prompt", () => {
    expect(gateGeneration({ id: "u" }, { prompt: "a child standing in a field", negative: "clothing" }).ok).toBe(false);
    expect(gateGeneration({ id: "u" }, { prompt: "a lighthouse at dusk", negative: "blurry" }).ok).toBe(true);
  });

  // The same vectors as apps/tui's logic.test.ts: one grammar for /video everywhere.
  test("/video grammar (shared with the TUI)", () => {
    expect(parseVideoCommand("/video a paper boat")).toEqual({ prompt: "a paper boat", model: "wan2.1-t2v-1.3b" });
    expect(parseVideoCommand("/clip --model wan2.2-ti2v-5b waves")).toEqual({ prompt: "waves", model: "wan2.2-ti2v-5b" });
    expect(parseVideoCommand("/vid waves --model=wan2.2-ti2v-5b")).toEqual({ prompt: "waves", model: "wan2.2-ti2v-5b" });
    expect(parseVideoCommand("/video --model nope waves")).toContain("model must be one of");
    expect(parseVideoCommand("/video")).toBe("usage: /video <what happens in the clip>");
    expect(parseVideoCommand("/video --model wan2.2-ti2v-5b")).toBe("usage: /video <what happens in the clip>");
    expect(parseVideoCommand(`/video ${"x".repeat(2001)}`)).toBe("prompt must contain 1 to 2000 characters");
    expect(parseVideoCommand("hello")).toBeNull();
    expect(parseVideoCommand("/videos cats")).toBeNull();
    expect(parseVideoCommand("/video-promo https://c0mpute.com")).toBeNull();
    // Whatever the chat sends passes the API's own validation.
    const cmd = parseVideoCommand("/video a paper boat on a pond") as { prompt: string; model: string };
    expect(parseVideoRequest(videoRequestBody(cmd, false))).toEqual({ prompt: "a paper boat on a pond", model: "wan2.1-t2v-1.3b" });
  });

  test("adult is sent only in 18+ mode, and only as true", () => {
    const cmd = { prompt: "a paper boat", model: "wan2.1-t2v-1.3b" };
    expect(videoRequestBody(cmd, false)).toEqual({ prompt: "a paper boat", model: "wan2.1-t2v-1.3b" });
    expect(videoRequestBody(cmd, false)).not.toHaveProperty("adult");
    expect(videoRequestBody(cmd, true)).toEqual({ prompt: "a paper boat", model: "wan2.1-t2v-1.3b", adult: true });
    expect(parseVideoRequest(videoRequestBody(cmd, true))).toMatchObject({ adult: true });
  });

  test("progress line: stage, place in line, worker, ETA", () => {
    expect(describeVideoProgress({ stage: "queued", queue_position: 3 })).toBe("queued for a c0mpute worker (#3 in line), it starts when one is free");
    expect(describeVideoProgress({ stage: "rendering", detail: "step 12/30", worker: "12D3KooW", eta_s: 600 })).toBe("rendering · step 12/30 · worker 12D3KooW · about 10 min left");
    expect(describeVideoProgress({})).toContain("queued");
  });

  test("polling survives deploys but stops on a real failure", () => {
    expect(videoPollOutcome(202, { status: "pending" })).toEqual({ kind: "pending" });
    expect(videoPollOutcome(200, { status: "done" })).toEqual({ kind: "done" });
    expect(videoPollOutcome(0, null)).toEqual({ kind: "retry" });
    expect(videoPollOutcome(503, null)).toEqual({ kind: "retry" });
    expect(videoPollOutcome(502, { error: { message: "network unreachable", retry: true } })).toEqual({ kind: "retry" });
    expect(videoPollOutcome(502, { error: { message: "worker crashed" } })).toEqual({ kind: "failed", message: "worker crashed" });
    expect(videoPollOutcome(404, { error: { message: "This video job expired" } })).toEqual({ kind: "failed", message: "This video job expired" });
    expect(videoPollOutcome(422, { error: { message: "refused by the content policy" } })).toEqual({ kind: "failed", message: "refused by the content policy" });
  });
});
