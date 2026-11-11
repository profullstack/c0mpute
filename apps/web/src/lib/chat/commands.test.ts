import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { adultModeActive, CHAT_COMMANDS, slashCommand, slashReplyText } from "./commands";
import { parseImageCommand } from "./images";
import { parseMusicCommand } from "./music";
import { parseAlbumCommand } from "./album";
import { detectPromoRequest, parsePromoCommand } from "./promo";
import { parseConnectCommand, parsePushCommand } from "./push-shared";
import { parseVideoCommand } from "./videos";

/** What the chat does with a line, in send()'s order: a command kind, or "llm". */
function route(text: string): string {
  if (parsePushCommand(text)) return "push";
  if (parseConnectCommand(text)) return "connect";
  if (parsePromoCommand(text) ?? detectPromoRequest(text)) return "promo";
  if (parseAlbumCommand(text)) return "album";
  if (parseMusicCommand(text)) return "music";
  if (parseImageCommand(text)) return "image";
  const v = parseVideoCommand(text);
  if (typeof v === "string") return "video-usage";
  if (v) return "video";
  const s = slashCommand(text);
  return s ? s.kind : "llm";
}

describe("slash commands never reach the model", () => {
  test("generation commands route to their handlers", () => {
    expect(route("/video a paper boat on a pond")).toBe("video");
    expect(route("/clip waves")).toBe("video");
    expect(route("/VID waves")).toBe("video");
    expect(route("/video")).toBe("video-usage");
    expect(route("/image a cat")).toBe("image");
    expect(route("/video-promo https://c0mpute.com")).toBe("promo");
    expect(route("/push hosting ls")).toBe("push");
  });

  test("an unknown command shows the list, not an LLM reply", () => {
    expect(route("/notacommand hello")).toBe("unknown");
    expect(slashCommand("/notacommand hello")).toEqual({ kind: "unknown", name: "notacommand" });
    const text = slashReplyText({ kind: "unknown", name: "notacommand" });
    expect(text).toContain("unknown command /notacommand");
    for (const [c] of CHAT_COMMANDS) expect(text).toContain(c);
    expect(route("/")).toBe("unknown");
    expect(route("/videos of cats")).toBe("unknown");
  });

  test("/help lists /video; a bare generation command gets its usage", () => {
    expect(route("/help")).toBe("help");
    expect(slashReplyText({ kind: "help" })).toContain("/video <prompt>");
    expect(route("/image")).toBe("usage");
    expect(route("/music")).toBe("usage");
    expect(route("/promo")).toBe("usage");
  });

  test("plain messages still go to the model", () => {
    expect(route("hello there")).toBe("llm");
    expect(route("make a video of a boat")).toBe("llm");
    expect(slashCommand("  what does /video do?")).toBeNull();
  });
});

describe("adult flag wiring", () => {
  test("18+ mode needs the attestation and the switch", () => {
    expect(adultModeActive(true, "2026-11-01T00:00:00Z")).toBe(true);
    expect(adultModeActive(false, "2026-11-01T00:00:00Z")).toBe(false);
    expect(adultModeActive(true, null)).toBe(false);
    expect(adultModeActive(true, undefined)).toBe(false);
  });

  test("the chat sends /video's body the way /image sends its own", () => {
    const src = readFileSync(join(import.meta.dir, "../../app/chat/chat-app.tsx"), "utf8");
    expect(src).toContain("const adultMode = adultModeActive(adultPref, me?.adult_verified_at);");
    expect(src).toContain('body: JSON.stringify(adult ? { prompt, adult: true } : { prompt })');
    expect(src).toContain("body: JSON.stringify(videoRequestBody(cmd, adult))");
    expect(src).toMatch(/async function makeVideo[\s\S]*?const adult = adultMode;/);
  });
});
