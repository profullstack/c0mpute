import { describe, expect, test } from "bun:test";
import { gateGeneration } from "./adult";
import { MAX_WAIT_S, parseVideoRequest, preferWait } from "./videos";

describe("videos", () => {
  test("defaults to Wan 2.1 1.3B, text only", () => {
    expect(parseVideoRequest({ prompt: " a lighthouse at dusk " })).toEqual({ prompt: "a lighthouse at dusk", model: "wan2.1-t2v-1.3b" });
    expect(typeof parseVideoRequest({ prompt: "x", model: "sora" })).toBe("string");
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
});
