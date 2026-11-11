// Albums in c0mpute chat: `/album <prompt>` has Infernet name a band, an
// album and 8-12 songs, write their lyrics, draw a cover and render every
// track on c0mpute workers (ACE-Step 1.5). Infernet's /api/chat/album owns
// the work, the free tier and per-track billing; c0mpute.com forwards and
// re-shapes what comes back.

import { sanitizeNodeProgress, type NodeProgress } from "./music";

export type AlbumCommand = { prompt: string };

/** `/album dark synthwave about a drowned city` (also /lp) -> { prompt }; anything else -> null. */
export function parseAlbumCommand(text: string): AlbumCommand | null {
  const m = /^\/(?:album|lp)\s+([\s\S]+)$/i.exec(String(text ?? "").trim());
  if (!m) return null;
  const prompt = m[1].trim();
  return prompt ? { prompt } : null;
}

export const MAX_PROMPT_CHARS = 100_000;
export const MIN_TRACKS = 2;
export const MAX_TRACKS = 12;

/** Validate a POST body; returns the body to forward or an error string. */
export function parseAlbumRequest(body: unknown): { prompt: string; tracks?: number; duration?: number } | string {
  const b = (body ?? {}) as Record<string, unknown>;
  const prompt = typeof b.prompt === "string" ? b.prompt.trim() : "";
  if (!prompt || prompt.length > MAX_PROMPT_CHARS) return `prompt must contain 1 to ${MAX_PROMPT_CHARS} characters`;
  const out: { prompt: string; tracks?: number; duration?: number } = { prompt };
  if (b.tracks !== undefined && b.tracks !== null) {
    const n = Number(b.tracks);
    if (!Number.isInteger(n) || n < MIN_TRACKS || n > MAX_TRACKS) return `an album has ${MIN_TRACKS} to ${MAX_TRACKS} tracks`;
    out.tracks = n;
  }
  if (b.duration !== undefined && b.duration !== null) {
    const d = Number(b.duration);
    if (!Number.isInteger(d) || d < 10 || d > 600) return "duration must be a whole number of seconds from 10 to 600";
    out.duration = d;
  }
  return out;
}

export type AlbumTrack = {
  n: number;
  title: string;
  duration: number | null;
  status: "planned" | "written" | "pending" | "done" | "failed";
  /** The song job: its audio is GET /api/v1/music/<id> once done. */
  id: string | null;
  lyrics: string | null;
  error: string | null;
  node?: NodeProgress;
};

export type AlbumView = {
  stage: string;
  tracksWanted: number | null;
  band: string | null;
  album: string | null;
  genre: string | null;
  concept: string | null;
  cover: boolean;
  tracks: AlbumTrack[];
};

const STATUSES = ["planned", "written", "pending", "done", "failed"];
const JOB_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** Keep only known, well-typed fields of Infernet's album poll body. */
export function sanitizeAlbum(v: unknown): AlbumView {
  const o = (v && typeof v === "object" ? v : {}) as Record<string, unknown>;
  const s = (x: unknown, max: number) => (typeof x === "string" && x.trim() ? x.slice(0, max) : null);
  const tracks = (Array.isArray(o.tracks) ? o.tracks : []).slice(0, MAX_TRACKS).map((raw, i): AlbumTrack => {
    const t = (raw && typeof raw === "object" ? raw : {}) as Record<string, unknown>;
    const lyrics = s(t.lyrics, 8000);
    return {
      n: Number.isInteger(t.n) ? (t.n as number) : i + 1,
      title: s(t.title, 80) ?? `Track ${i + 1}`,
      duration: typeof t.duration === "number" && Number.isFinite(t.duration) ? t.duration : null,
      status: (STATUSES.includes(t.status as string) ? t.status : "planned") as AlbumTrack["status"],
      id: typeof t.id === "string" && JOB_ID.test(t.id) ? t.id : null,
      lyrics: lyrics && !/^\s*\[instrumental\]\s*$/i.test(lyrics) ? lyrics : null,
      error: s(t.error, 300),
      node: sanitizeNodeProgress(t.node),
    };
  });
  return {
    stage: s(o.stage, 20) ?? "starting",
    tracksWanted: typeof o.tracksWanted === "number" ? o.tracksWanted : null,
    band: s(o.band, 80),
    album: s(o.album, 100),
    genre: s(o.genre, 100),
    concept: s(o.concept, 800),
    cover: o.cover === "ready",
    tracks,
  };
}

/** One status line for the chat from an album poll. */
export function describeAlbumStage(a: AlbumView): string {
  const total = a.tracks.length || a.tracksWanted || 0;
  switch (a.stage) {
    case "planning":
      return `naming the band, the album and ${total || "its"} songs`;
    case "writing":
      return `writing lyrics · ${a.tracks.filter((t) => t.lyrics || t.status !== "planned").length}/${total} songs`;
    case "recording": {
      const done = a.tracks.filter((t) => t.status === "done").length;
      const failed = a.tracks.filter((t) => t.status === "failed").length;
      const now = a.tracks.filter((t) => t.status === "pending" && t.node?.stage === "rendering").length;
      const waiting = a.tracks.filter((t) => t.status === "pending" && t.node?.stage !== "rendering").length;
      return `recording on c0mpute workers · ${done}/${total} done${now ? ` · ${now} rendering` : ""}${waiting ? ` · ${waiting} waiting for a worker` : ""}${failed ? ` · ${failed} failed` : ""}`;
    }
    case "cover":
      return "songs done; finishing the cover";
    case "done":
      return "album ready";
    default:
      return "starting the album";
  }
}

/** "01 - Neon Rain": a track's file name without the extension. */
export function trackFileBase(n: number, title: string): string {
  const safe = String(title ?? "")
    // eslint-disable-next-line no-control-regex -- strip control characters
    .replace(/[\u0000-\u001f\u007f<>:"/\\|?*]+/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 80);
  return `${String(n).padStart(2, "0")} - ${safe || `Track ${n}`}`;
}
