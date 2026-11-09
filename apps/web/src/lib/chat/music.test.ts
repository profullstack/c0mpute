import { describe, expect, test } from "bun:test";
import { audioExtension, parseMusicCommand, parseMusicRequest } from "./music";

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
    expect(parseMusicRequest({ prompt: " jazz " })).toEqual({ prompt: "jazz", lyrics: "", duration: 30 });
    expect(parseMusicRequest({ prompt: "x", duration: 60, lyrics: "hi" })).toEqual({ prompt: "x", lyrics: "hi", duration: 60 });
    expect(typeof parseMusicRequest({ prompt: "x", duration: 5 })).toBe("string");
    expect(typeof parseMusicRequest({ prompt: "x", duration: 200 })).toBe("string");
    expect(typeof parseMusicRequest({ prompt: "" })).toBe("string");
    expect(typeof parseMusicRequest(null)).toBe("string");
  });
  test("file extensions follow the content type", () => {
    expect(audioExtension("audio/mpeg")).toBe("mp3");
    expect(audioExtension("audio/wav")).toBe("wav");
    expect(audioExtension("")).toBe("mp3");
  });
});
