/**
 * What a line typed into the chat means. Generation commands are parsed by
 * the web chat's own code (src/web/, copied verbatim from apps/web and kept
 * identical by a test), in the same order the web chat tries them, so a line
 * does the same thing in the browser, in `c0mpute chat` and here.
 */
import { parseAlbumCommand, type AlbumCommand } from "./web/album.ts";
import { parseImageCommand } from "./web/images.ts";
import { parseMusicCommand, type MusicCommand } from "./web/music.ts";
import { detectPromoRequest, parsePromoCommand, type PromoCommand } from "./web/promo.ts";
import { parseConnectCommand, parsePushCommand, type ConnectCommand, type PushCommand } from "./web/push-shared.ts";
import { DEFAULT_VIDEO_MODEL, VIDEO_MODELS } from "./web/videos.ts";

export type VideoCommand = { prompt: string; model: string };

export type Command =
  | { kind: "chat"; text: string }
  | { kind: "image"; prompt: string }
  | { kind: "video"; video: VideoCommand }
  | { kind: "music"; music: MusicCommand }
  | { kind: "album"; album: AlbumCommand }
  | { kind: "promo"; promo: PromoCommand }
  | { kind: "push"; push: PushCommand }
  | { kind: "connect"; connect: ConnectCommand }
  | { kind: "model"; model: string | null }
  | { kind: "models" }
  | { kind: "new" }
  | { kind: "help" }
  | { kind: "jobs" }
  | { kind: "error"; message: string };

/**
 * `/video a paper boat on a pond` (also /clip, /vid): text to video on CPU
 * workers via POST /api/v1/videos. `--model wan2.2-ti2v-5b` picks the GPU model.
 */
export function parseVideoCommand(text: string): VideoCommand | string | null {
  const m = /^\/(?:video|clip|vid)(?:\s+([\s\S]*))?$/i.exec(String(text ?? "").trim());
  if (!m) return null;
  let rest = (m[1] ?? "").trim();
  let model = DEFAULT_VIDEO_MODEL;
  const flag = /(?:^|\s)--model[=\s]+(\S+)/.exec(rest);
  if (flag) {
    if (!(VIDEO_MODELS as readonly string[]).includes(flag[1]!)) return `model must be one of: ${VIDEO_MODELS.join(", ")}`;
    model = flag[1]!;
    rest = rest.replace(flag[0], " ").trim();
  }
  if (!rest) return "usage: /video <what happens in the clip>";
  if (rest.length > 2000) return "prompt must contain 1 to 2000 characters";
  return { prompt: rest, model };
}

/**
 * The web chat takes lyrics on the lines after `lyrics:`; a terminal input is
 * one line, so `lyrics:` may also appear inline: `/music lo-fi lyrics: la la`.
 */
export function musicLine(text: string): string {
  return text.replace(/\s+lyrics:\s*/i, "\nlyrics:\n");
}

export const HELP_COMMANDS: [string, string][] = [
  ["/image <prompt>", "draw a picture on a c0mpute CPU worker (~2 min)"],
  ["/video <prompt>", "a short clip (Wan 2.1 on CPU, 20-30 min once a worker is free)"],
  ["/music <style> [lyrics: …]", "a song (ACE-Step 1.5); no lyrics = instrumental"],
  ["/album <idea>", "a band, 8 named songs with lyrics, cover art and a zip"],
  ["/promo <url> [pixar|anime|…] [vertical]", "a promo video with narration and music"],
  ["/push hosting <name> | ls | rm <name>", "publish this chat's files as a site on the network"],
  ["/connect hosting [node]", "link or pick the node that publishes your sites"],
  ["/model [id]", "pick the model (no id: open the picker)"],
  ["/models", "list the live models"],
  ["/new", "start a new conversation"],
  ["/jobs", "go to the Jobs tab"],
  ["/help", "this list"],
];

export function parseLine(input: string): Command | null {
  const text = input.trim();
  if (!text) return null;
  const push = parsePushCommand(text);
  if (push) return { kind: "push", push };
  const connect = parseConnectCommand(text);
  if (connect) return { kind: "connect", connect };
  const promo = parsePromoCommand(text) ?? detectPromoRequest(text);
  if (promo) return { kind: "promo", promo };
  const album = parseAlbumCommand(text);
  if (album) return { kind: "album", album };
  const music = parseMusicCommand(musicLine(text));
  if (music) return { kind: "music", music };
  const image = parseImageCommand(text);
  if (image) return { kind: "image", prompt: image };
  const video = parseVideoCommand(text);
  if (typeof video === "string") return { kind: "error", message: video };
  if (video) return { kind: "video", video };
  const local = /^\/(\w+)(?:\s+([\s\S]*))?$/.exec(text);
  if (local) {
    const [, name, arg] = local;
    switch (name!.toLowerCase()) {
      case "model":
        return { kind: "model", model: arg?.trim() || null };
      case "models":
        return { kind: "models" };
      case "new":
      case "reset":
      case "clear":
        return { kind: "new" };
      case "help":
      case "h":
      case "?":
        return { kind: "help" };
      case "jobs":
        return { kind: "jobs" };
      case "image":
      case "img":
      case "draw":
        return { kind: "error", message: "usage: /image <what to draw>" };
      case "music":
      case "song":
        return { kind: "error", message: "usage: /music <style> [lyrics: <words>]" };
      case "album":
      case "lp":
        return { kind: "error", message: "usage: /album <idea for the album>" };
      case "promo":
        return { kind: "error", message: "usage: /promo <url> [pixar|anime|claymation|flat|watercolor|comic|cinematic] [vertical]" };
      default:
        return { kind: "error", message: `unknown command /${name}; /help lists them` };
    }
  }
  return { kind: "chat", text };
}
