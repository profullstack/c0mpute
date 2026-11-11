import { describe, expect, test } from "bun:test";
import { audioExtension, describeNodeProgress, parseMusicCommand, parseMusicRequest, sanitizeNodeProgress } from "./music";

describe("chat music", () => {
  test("parses /music and /song, with optional lyrics", () => {
    expect(parseMusicCommand("/music lo-fi hip hop")).toEqual({ prompt: "lo-fi hip hop", lyrics: "" });
    expect(parseMusicCommand(" /SONG synthpop ")).toEqual({ prompt: "synthpop", lyrics: "" });
    expect(parseMusicCommand("/music synthpop\nlyrics:\n[verse]\nneon")).toEqual({ prompt: "synthpop", lyrics: "[verse]\nneon" });
    expect(parseMusicCommand("/music folk\n\nla la")).toEqual({ prompt: "folk", lyrics: "la la" });
    expect(parseMusicCommand("/music")).toBeNull();
    expect(parseMusicCommand("/musical")).toBeNull();
    expect(parseMusicCommand("play music")).toBeNull();
  });
  test("validates the request", () => {
    expect(parseMusicRequest({ prompt: " jazz " })).toEqual({ prompt: "jazz", lyrics: "" });
    expect(parseMusicRequest({ prompt: "x", duration: 60, lyrics: "hi" })).toEqual({ prompt: "x", lyrics: "hi", duration: 60 });
    expect(typeof parseMusicRequest({ prompt: "x", duration: 5 })).toBe("string");
    expect(typeof parseMusicRequest({ prompt: "x", duration: 601 })).toBe("string");
    expect(typeof parseMusicRequest({ prompt: "" })).toBe("string");
    expect(parseMusicRequest({ prompt: "x".repeat(100_000), lyrics: "y".repeat(100_000) })).toMatchObject({ lyrics: "y".repeat(100_000) });
    expect(typeof parseMusicRequest({ prompt: "x".repeat(100_001) })).toBe("string");
    expect(typeof parseMusicRequest(null)).toBe("string");
  });
  test("file extensions follow the content type", () => {
    expect(audioExtension("audio/mpeg")).toBe("mp3");
    expect(audioExtension("audio/wav")).toBe("wav");
    expect(audioExtension("")).toBe("mp3");
  });
  test("node progress is sanitized and described", () => {
    expect(sanitizeNodeProgress({ stage: "evil" })).toBeUndefined();
    expect(sanitizeNodeProgress(null)).toBeUndefined();
    const p = sanitizeNodeProgress({ stage: "rendering", worker: "12D3KooWG9TmqCHN63oFoxb2bwTGRYqwW8e1C2Ua9e14XtPja8R9", eta_s: 600, stage_elapsed_s: 125, attempt: 1, junk: 1 });
    expect(p).toMatchObject({ stage: "rendering", eta_s: 600 });
    expect(describeNodeProgress(p!)).toBe("rendering on node …XtPja8R9 · 2:05 in · ~7:55 left (node's estimate)");
    expect(describeNodeProgress({ stage: "queued", stage_elapsed_s: 61 })).toBe("every music worker is busy; queued 1:01");
    expect(describeNodeProgress({ stage: "retrying", worker: "abcdefgh12345678", detail: "worker failed" })).toContain("trying another worker");
  });
});
