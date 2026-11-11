// The web chat's slash commands: the list /help shows, and what a line that
// starts with "/" but matches no generation command means. Such a line never
// goes to the LLM: it is /help, a usage hint for a command typed without its
// argument, or "unknown command".

export const CHAT_COMMANDS: [string, string][] = [
  ["/image <prompt>", "draw a picture on a c0mpute CPU worker (~2 min)"],
  ["/video <prompt> [--model wan2.2-ti2v-5b]", "a short clip (Wan 2.1 on CPU, 20-30 min once a worker is free; also /clip, /vid)"],
  ["/music <style>", "a song (ACE-Step 1.5); a line \"lyrics:\" then the words adds vocals"],
  ["/album <idea>", "a band, 8 named songs with lyrics, cover art and a zip"],
  ["/promo <url> [pixar|anime|claymation|flat] [vertical]", "a promo video with narration and music"],
  ["/push hosting <name> | ls | rm <name>", "publish this chat's files as a site on the c0mpute network"],
  ["/connect hosting [node]", "link or pick the node that publishes your sites"],
  ["/help", "this list"],
];

export function commandList(): string {
  const w = Math.max(...CHAT_COMMANDS.map(([c]) => c.length));
  return CHAT_COMMANDS.map(([c, d]) => `${c.padEnd(w)}  ${d}`).join("\n");
}

const USAGE: Record<string, string> = {
  image: "usage: /image <what to draw>",
  img: "usage: /image <what to draw>",
  draw: "usage: /image <what to draw>",
  music: "usage: /music <style> (then a line \"lyrics:\" and the words for vocals)",
  song: "usage: /music <style> (then a line \"lyrics:\" and the words for vocals)",
  album: "usage: /album <idea for the album>",
  lp: "usage: /album <idea for the album>",
  promo: "usage: /promo <url> [pixar|anime|claymation|flat|watercolor|comic|cinematic] [vertical]",
  "promo-video": "usage: /promo <url> [pixar|anime|claymation|flat|watercolor|comic|cinematic] [vertical]",
  "demo-video": "usage: /promo <url> [pixar|anime|claymation|flat|watercolor|comic|cinematic] [vertical]",
  "video-promo": "usage: /promo <url> [pixar|anime|claymation|flat|watercolor|comic|cinematic] [vertical]",
};

export type SlashReply = { kind: "help" } | { kind: "usage"; message: string } | { kind: "unknown"; name: string };

/**
 * Call after every generation parser has said no. Any line starting with "/"
 * gets a local reply here; anything else (null) is a chat message.
 */
export function slashCommand(text: string): SlashReply | null {
  const s = String(text ?? "").trim();
  if (!s.startsWith("/")) return null;
  const name = (/^\/(\S*)/.exec(s)?.[1] ?? "").toLowerCase();
  if (name === "help" || name === "h" || name === "?" || name === "commands") return { kind: "help" };
  if (USAGE[name]) return { kind: "usage", message: USAGE[name] };
  return { kind: "unknown", name };
}

/** The text a local slash reply shows in the chat. */
export function slashReplyText(r: SlashReply): string {
  if (r.kind === "help") return `commands:\n${commandList()}\n\nAnything else is a message to the model.`;
  if (r.kind === "usage") return `// ${r.message}`;
  return `// unknown command /${r.name}\n\ncommands:\n${commandList()}`;
}

/** 18+ mode is on only for an account with the attestation AND this browser's switch on. */
export const adultModeActive = (pref: boolean, adultVerifiedAt: string | null | undefined) => pref && !!adultVerifiedAt;
