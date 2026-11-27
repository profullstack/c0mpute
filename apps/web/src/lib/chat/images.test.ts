import { describe, expect, test } from "bun:test";
import { isJobId, parseImageCommand, parseImageRequest, textHistory } from "./images";

describe("chat images", () => {
  test("parses /image, /img, /draw only", () => {
    expect(parseImageCommand("/image a red bicycle")).toBe("a red bicycle");
    expect(parseImageCommand(" /IMG fox ")).toBe("fox");
    expect(parseImageCommand("/draw\ncat")).toBe("cat");
    expect(parseImageCommand("draw a cat")).toBeNull();
    expect(parseImageCommand("/image")).toBeNull();
  });
  test("validates the request", () => {
    expect(parseImageRequest({ prompt: " x " })).toEqual({ prompt: "x", size: "landscape" });
    expect(parseImageRequest({ prompt: "x", size: "portrait" })).toEqual({ prompt: "x", size: "portrait" });
    expect(typeof parseImageRequest({ prompt: "" })).toBe("string");
    expect(typeof parseImageRequest({ prompt: "x", size: "huge" })).toBe("string");
    expect(typeof parseImageRequest(null)).toBe("string");
    // ChatrXL by field or by a --model flag inside the prompt (chat, TUI).
    expect(parseImageRequest({ prompt: "a fox --model chatrxl-uncensored" })).toEqual({ prompt: "a fox", size: "landscape", model: "chatrxl-uncensored" });
    expect(parseImageRequest({ prompt: "a fox", model: "chatrxl-uncensored", adult: true })).toEqual({ prompt: "a fox", size: "landscape", model: "chatrxl-uncensored", adult: true });
    expect(parseImageRequest({ prompt: "a fox", model: "z-image-turbo" })).toEqual({ prompt: "a fox", size: "landscape" });
    expect(typeof parseImageRequest({ prompt: "a fox --model flux" })).toBe("string");
    expect(typeof parseImageRequest({ prompt: "a fox", model: "flux" })).toBe("string");
    expect(typeof parseImageRequest({ prompt: "--model chatrxl-uncensored" })).toBe("string");
  });
  test("job ids are uuids", () => {
    expect(isJobId("c653a135-1f8f-4b76-8066-01c0175f73f8")).toBe(true);
    expect(isJobId("../etc")).toBe(false);
  });
  test("image turns stay out of the model history", () => {
    const h = textHistory([{ content: "hi" }, { content: "[image]", imageTurn: true }, { content: "ok" }]);
    expect(h.map((m) => m.content)).toEqual(["hi", "ok"]);
  });
});
