// Client-safe half of /push: command parsing, limits and file collection.
// No Node imports here: chat-app.tsx bundles this for the browser.

export const TARGETS = ["hosting"] as const;
export type Target = (typeof TARGETS)[number];

// ── limits + validation ─────────────────────────────────────────────────────

/** Same rule as `c0mpute hosting publish --name`: 1-63 of a-z, 0-9 and '-'. */
export const SITE_NAME = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
export const NODE_NAME = SITE_NAME;
export const MAX_FILES = 200;
export const MAX_FILE_BYTES = 256 * 1024 * 1024;
export const MAX_PUSH_BYTES = 512 * 1024 * 1024;
export const MAX_NODES = 10;
export const MAX_PUSHES = 100;
/** A node that claimed a job and went quiet this long loses it to the next poll. */
export const CLAIM_TIMEOUT_MS = 2 * 60 * 60_000;
/** A node is "online" if it polled this recently. */
export const ONLINE_MS = 2 * 60_000;

const SEGMENT = /^[A-Za-z0-9_()+,=@ -][A-Za-z0-9._()+,=@ -]{0,127}$/;

/**
 * A relative path inside the site: 1-4 segments, no "..", no hidden files,
 * no backslashes. Spaces are fine (album tracks are "01 - Title.mp3").
 * Returns the normalized path or null.
 */
export function cleanPath(raw: unknown): string | null {
  if (typeof raw !== "string") return null;
  const p = raw.replace(/^\/+/, "").trim();
  if (!p || p.length > 400 || p.includes("\\") || p.includes("\0")) return null;
  const parts = p.split("/");
  if (parts.length > 4) return null;
  for (const s of parts) if (!SEGMENT.test(s) || s.trim() !== s || /^\.+$/.test(s)) return null;
  return parts.join("/");
}

/** Parse `/push hosting <name>`, `/push hosting ls`, `/push hosting rm <name>`; `/push <name>` = hosting. */
export type PushCommand =
  | { kind: "push"; target: Target; name: string }
  | { kind: "ls"; target: Target }
  | { kind: "rm"; target: Target; name: string }
  | { kind: "help"; error?: string };

export function parsePushCommand(text: string): PushCommand | null {
  const m = /^\/push(?:\s+([\s\S]*))?$/i.exec(String(text ?? "").trim());
  if (!m) return null;
  const words = (m[1] ?? "").trim().split(/\s+/).filter(Boolean);
  if (words[0]?.toLowerCase() === "hosting") words.shift();
  const [a, b] = words;
  if (!a) return { kind: "help" };
  const verb = a.toLowerCase();
  if (verb === "ls" || verb === "list") return { kind: "ls", target: "hosting" };
  if (verb === "rm" || verb === "delete") {
    const name = (b ?? "").toLowerCase();
    return SITE_NAME.test(name) ? { kind: "rm", target: "hosting", name } : { kind: "help", error: "usage: /push hosting rm <name>" };
  }
  const name = a.toLowerCase();
  if (!SITE_NAME.test(name)) return { kind: "help", error: `"${a}" is not a site name: use 1-63 of a-z, 0-9 and '-'` };
  return { kind: "push", target: "hosting", name };
}

/** `/connect hosting [node]` (also `/connect [node]`). */
export type ConnectCommand = { target: Target; node: string | null; error?: string };

export function parseConnectCommand(text: string): ConnectCommand | null {
  const m = /^\/connect(?:\s+([\s\S]*))?$/i.exec(String(text ?? "").trim());
  if (!m) return null;
  const words = (m[1] ?? "").trim().split(/\s+/).filter(Boolean);
  if (words[0]?.toLowerCase() === "hosting") words.shift();
  const node = words[0]?.toLowerCase() ?? null;
  if (node && !NODE_NAME.test(node)) return { target: "hosting", node: null, error: `"${words[0]}" is not a node name` };
  return { target: "hosting", node };
}

// ── files ───────────────────────────────────────────────────────────────────

/**
 * One file of a push. Media is a reference to one of our own result routes
 * (`src`), fetched by the node through c0mpute.com with the account's key, so
 * nothing big is staged on c0mpute.com. Small text (the transcript page) is
 * inline (`text`).
 */
export type PushFile = { path: string; type: string; src?: string; text?: string };

/** The result routes a push may reference: images, songs, covers, album zips, promo videos. */
export const SRC =
  /^\/api\/v1\/(?:images\/[A-Za-z0-9_-]{1,80}|music\/[A-Za-z0-9_-]{1,80}(?:\/cover)?|album\/[A-Za-z0-9_-]{1,80}\/(?:zip|cover)|promo\/[A-Za-z0-9_-]{1,80}\/(?:video|captions))$/;
export const MAX_INLINE_BYTES = 1024 * 1024;

const utf8Len = (s: string) => new TextEncoder().encode(s).length;

/** Validate a client's file list; returns the clean list or an error. */
export function validatePushFiles(raw: unknown): PushFile[] | string {
  if (!Array.isArray(raw) || raw.length === 0) return "nothing to push: this chat has no files yet";
  if (raw.length > MAX_FILES) return `at most ${MAX_FILES} files per push`;
  const seen = new Set<string>();
  const out: PushFile[] = [];
  let inline = 0;
  for (const f of raw as Record<string, unknown>[]) {
    const path = cleanPath(f?.path);
    if (!path) return `bad file path ${JSON.stringify(f?.path)}`;
    if (seen.has(path.toLowerCase())) return `duplicate file ${path}`;
    seen.add(path.toLowerCase());
    const type = typeof f.type === "string" && /^[a-z]+\/[a-z0-9.+-]+$/i.test(f.type) ? f.type : "application/octet-stream";
    if (typeof f.src === "string") {
      if (!SRC.test(f.src)) return `${path}: not a c0mpute chat result`;
      out.push({ path, type, src: f.src });
    } else if (typeof f.text === "string") {
      inline += utf8Len(f.text);
      if (inline > MAX_INLINE_BYTES) return "inline files are over 1 MiB";
      out.push({ path, type, text: f.text });
    } else return `${path}: needs src or text`;
  }
  return out;
}

/** The parts of a chat message /push looks at (mirrors chat-app's Msg). */
export type PushMsg = {
  role: string;
  content: string;
  /** An adult (18+) result: never published, shared or pushed. */
  adult?: boolean;
  imageJob?: string;
  musicJob?: string;
  audioType?: string;
  promo?: { id: string; video?: string; captions?: string; title?: string };
  album?: { id: string; band: string | null; album: string | null; cover: string | null; zip: string | null };
};

const slug = (s: string, fallback: string) =>
  (s || "")
    .normalize("NFKD")
    .replace(/[^\w\s()+,=@-]/g, "")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 80) || fallback;

const esc = (s: string) =>
  s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);

/**
 * Everything a conversation produced, as push files: images/NN.png,
 * music/NN - Title.mp3 (+ cover), albums/<Band - Album>.zip, video/NN.mp4,
 * plus index.html (the transcript, showing each file) and chat.md.
 */
export function collectPushFiles(title: string, messages: PushMsg[]): PushFile[] {
  const files: PushFile[] = [];
  const at = new Map<number, PushFile[]>();
  const add = (i: number, f: PushFile) => {
    if (files.some((x) => x.path.toLowerCase() === f.path.toLowerCase())) return;
    files.push(f);
    at.set(i, [...(at.get(i) ?? []), f]);
  };
  let img = 0;
  let song = 0;
  let vid = 0;
  const two = (n: number) => String(n).padStart(2, "0");
  messages.forEach((m, i) => {
    if (m.role !== "assistant") return;
    // Adult results stay private to the person who made them.
    if (m.adult) return;
    if (m.imageJob) add(i, { path: `images/${two(++img)}.png`, type: "image/png", src: `/api/v1/images/${m.imageJob}` });
    if (m.musicJob) {
      // Song turns read `"Title" [music: caption ...]`; untitled ones are just "song".
      const name = slug(/^"([^"]{1,120})"/.exec(m.content)?.[1] ?? "", "song").slice(0, 60);
      const n = two(++song);
      const ext = m.audioType?.includes("wav") ? "wav" : m.audioType?.includes("flac") ? "flac" : "mp3";
      add(i, { path: `music/${n} - ${name}.${ext}`, type: m.audioType || "audio/mpeg", src: `/api/v1/music/${m.musicJob}` });
      add(i, { path: `music/${n} - ${name}.png`, type: "image/png", src: `/api/v1/music/${m.musicJob}/cover` });
    }
    if (m.album?.id && m.album.zip) {
      const name = slug([m.album.band, m.album.album].filter(Boolean).join(" - "), `album ${m.album.id.slice(0, 6)}`);
      add(i, { path: `albums/${name}.zip`, type: "application/zip", src: `/api/v1/album/${m.album.id}/zip` });
      if (m.album.cover) add(i, { path: `albums/${name}.png`, type: "image/png", src: `/api/v1/album/${m.album.id}/cover` });
    }
    if (m.promo?.id && m.promo.video) {
      const n = two(++vid);
      add(i, { path: `video/${n}.mp4`, type: "video/mp4", src: `/api/v1/promo/${m.promo.id}/video` });
      if (m.promo.captions) add(i, { path: `video/${n}.vtt`, type: "text/vtt", src: `/api/v1/promo/${m.promo.id}/captions` });
    }
  });

  const shown = messages.map((m, i) => ({ m, i })).filter(({ m }) => m.role !== "system");
  const md = shown.map(({ m }) => `**${m.role === "user" ? "you" : "c0mpute"}:** ${m.content}`).join("\n\n");
  const body = shown
    .map(({ m, i }) => {
      const media = (at.get(i) ?? [])
        .map((f) => {
          const href = f.path.split("/").map(encodeURIComponent).join("/");
          if (f.type.startsWith("image/")) return `<a href="${href}"><img src="${href}" alt="" loading="lazy"></a>`;
          if (f.type.startsWith("audio/"))
            return `<audio controls preload="none" src="${href}"></audio> <a href="${href}" download>${esc(f.path)}</a>`;
          if (f.type.startsWith("video/")) return `<video controls preload="none" src="${href}"></video>`;
          return `<a href="${href}" download>${esc(f.path)}</a>`;
        })
        .join("\n");
      return `<section id="m${i + 1}"><b>${m.role === "user" ? "you" : "c0mpute"}</b><div>${esc(m.content)}</div>${media}</section>`;
    })
    .join("\n");
  const t = esc(title || "c0mpute chat");
  const html = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>${t}</title>
<style>
:root{color-scheme:light dark;--bg:#fff;--fg:#111;--dim:#666;--line:#ddd}
@media (prefers-color-scheme:dark){:root{--bg:#0b0b0b;--fg:#eee;--dim:#999;--line:#333}}
body{background:var(--bg);color:var(--fg);font:15px/1.6 ui-monospace,SFMono-Regular,Menlo,monospace;max-width:52rem;margin:0 auto;padding:1.5rem 1rem}
section{border-top:1px solid var(--line);padding:.8rem 0}section b{color:var(--dim)}
section div{white-space:pre-wrap;overflow-wrap:anywhere}img,video{max-width:100%;height:auto;display:block;margin:.5rem 0}audio{width:100%}
footer{color:var(--dim);margin-top:2rem}a{color:inherit}
</style></head><body>
<h1>${t}</h1>
${body}
<footer>made in <a href="https://c0mpute.com/chat">c0mpute chat</a>, hosted on the c0mpute network · <a href="chat.md">chat.md</a></footer>
</body></html>
`;
  return [
    { path: "index.html", type: "text/html", text: html },
    { path: "chat.md", type: "text/markdown", text: `# ${title || "c0mpute chat"}\n\n${md}\n` },
    ...files,
  ];
}

