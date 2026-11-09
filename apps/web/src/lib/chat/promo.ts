// Promo videos in c0mpute chat: `/promo https://site pixar style`, or a plain
// "make a promo video for site.com". Infernet's /api/chat/promo does the work
// (brand fetch, storyboard, keyframes, Kokoro narration and an ACE-Step
// soundtrack on c0mpute workers, ffmpeg cut) and owns the free tier and
// billing; c0mpute.com forwards, naming the signed-in person.

export type PromoStyle = "pixar" | "anime" | "claymation" | "flat" | "watercolor" | "comic" | "cinematic";
export type PromoCommand = { url: string; style: PromoStyle; vertical: boolean; brief: string };

export const PROMO_STYLES: PromoStyle[] = ["pixar", "anime", "claymation", "flat", "watercolor", "comic", "cinematic"];
export const DEFAULT_STYLE: PromoStyle = "pixar";

const STYLE_ALIASES: [RegExp, PromoStyle][] = [
  [/\b(pixar|disney|dreamworks|3d cartoon|3-d cartoon|cgi cartoon)\b/i, "pixar"],
  [/\b(anime|manga|ghibli)\b/i, "anime"],
  [/\b(clay|claymation|stop[- ]?motion|plasticine|aardman)\b/i, "claymation"],
  [/\b(flat|2d|2-d|vector|motion graphics)\b/i, "flat"],
  [/\b(watercolou?r|storybook|painted)\b/i, "watercolor"],
  [/\b(comic|comic book|marvel|graphic novel)\b/i, "comic"],
  [/\b(cinematic|live[- ]action|photoreal(istic)?|realistic)\b/i, "cinematic"],
];

export function styleFromText(text: string): PromoStyle | null {
  for (const [re, key] of STYLE_ALIASES) if (re.test(text)) return key;
  return null;
}

const URL_RE = /\bhttps?:\/\/[^\s<>"'`)\]]+/i;
const BARE_DOMAIN_RE =
  /\b((?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+(?:com|net|org|io|ai|dev|app|co|sh|xyz|world|gg|me|tv|so|tech|site|cloud|us|uk|de|fr|ca))(\/[^\s<>"'`)\]]*)?/i;
const VERTICAL_RE = /\b(vertical|portrait|9:16|9x16|shorts?|reels?|tiktok|stories)\b/i;

function split(text: string): PromoCommand | null {
  const m = URL_RE.exec(text);
  const d = m ? null : BARE_DOMAIN_RE.exec(text);
  const url = m ? m[0].replace(/[.,;:!?]+$/, "") : d ? `https://${d[0].replace(/[.,;:!?]+$/, "")}` : null;
  if (!url) return null;
  return {
    url,
    style: styleFromText(text) ?? DEFAULT_STYLE,
    vertical: VERTICAL_RE.test(text),
    brief: text.replace(URL_RE, " ").replace(/\s+/g, " ").trim(),
  };
}

/** `/promo https://c0mpute.com pixar vertical` (also /promo-video, /demo-video). */
export function parsePromoCommand(text: string): PromoCommand | null {
  const m = /^\/(?:promo|promo-video|demo-video|video-promo)\s+([\s\S]+)$/i.exec(String(text ?? "").trim());
  return m ? split(m[1].trim()) : null;
}

/** "make a pixar style promo video for https://c0mpute.com": a link AND video words. */
export function detectPromoRequest(text: string): PromoCommand | null {
  const s = String(text ?? "").trim();
  if (!s || s.startsWith("/") || s.length > 2000) return null;
  const promoWord = /\b(promo|promotional|explainer|commercial|trailer|sizzle reel|advert|ad spot|demo video|launch video)\b/i.test(s);
  const videoWord = /\b(video|cartoon|animation|animated|clip|reel|short film)\b/i.test(s);
  const makeWord = /\b(make|create|generate|produce|build|render|do|want|need|give me|can you)\b/i.test(s);
  const ok = (promoWord && (videoWord || makeWord)) || (videoWord && makeWord && /\b(promo|demo|ad|cartoon|marketing|launch)\b/i.test(s));
  return ok ? split(s) : null;
}

/** Validate a POST body; returns the body to forward or an error string. */
export function parsePromoRequest(body: unknown): { url: string; style: PromoStyle; vertical: boolean; brief: string; voice?: string; music?: boolean } | string {
  const b = (body ?? {}) as Record<string, unknown>;
  const raw = typeof b.url === "string" ? b.url.trim() : "";
  if (!raw || raw.length > 2048) return "url is required (a public http or https link)";
  let url: URL;
  try {
    url = new URL(/^https?:\/\//i.test(raw) ? raw : `https://${raw}`);
  } catch {
    return "url is not a valid link";
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") return "url must be http or https";
  if (url.username || url.password) return "url must not contain credentials";
  if (!url.hostname.includes(".")) return "url must name a public host";
  const styleRaw = typeof b.style === "string" ? b.style.toLowerCase().trim() : "";
  const style = (PROMO_STYLES as string[]).includes(styleRaw) ? (styleRaw as PromoStyle) : styleFromText(styleRaw) ?? DEFAULT_STYLE;
  const brief = typeof b.brief === "string" ? b.brief.trim().slice(0, 2000) : "";
  const out: { url: string; style: PromoStyle; vertical: boolean; brief: string; voice?: string; music?: boolean } = {
    url: url.href,
    style,
    vertical: b.vertical === true || b.vertical === "true" || b.aspect === "9:16",
    brief,
  };
  if (typeof b.voice === "string") out.voice = b.voice.slice(0, 32);
  if (b.music === false) out.music = false;
  return out;
}

export const STAGES = ["brand", "script", "frames", "motion", "voice", "music", "render"] as const;
const STATES = ["pending", "running", "done", "failed", "skipped"];
export type StageState = { state: string; detail?: string };
export type PromoScene = { caption: string; narration: string; duration: number; frame: boolean; clip: boolean };
export type PromoProgress = {
  id: string;
  status: "pending" | "done" | "failed";
  seconds?: number;
  stage?: string;
  title?: string;
  tagline?: string;
  style?: string;
  vertical?: boolean;
  motion?: string;
  duration?: number;
  bytes?: number;
  brand?: { name?: string; host?: string; colors?: Record<string, string> };
  stages: Record<string, StageState>;
  scenes: PromoScene[];
  error?: string;
};

const str = (v: unknown, max: number) => (typeof v === "string" ? v.slice(0, max) : undefined);
const num = (v: unknown) => (typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : undefined);

/** Keep only known, well-typed fields of Infernet's poll body (it is relayed to the browser). */
export function sanitizePromoProgress(id: string, v: unknown): PromoProgress {
  const o = (v && typeof v === "object" ? v : {}) as Record<string, unknown>;
  const stagesIn = (o.stages && typeof o.stages === "object" ? o.stages : {}) as Record<string, unknown>;
  const stages: Record<string, StageState> = {};
  for (const k of STAGES) {
    const s = stagesIn[k] as Record<string, unknown> | undefined;
    if (s && typeof s.state === "string" && STATES.includes(s.state)) stages[k] = { state: s.state, ...(str(s.detail, 160) ? { detail: str(s.detail, 160) } : {}) };
  }
  const scenes = (Array.isArray(o.scenes) ? o.scenes : []).slice(0, 12).map((s) => {
    const x = (s ?? {}) as Record<string, unknown>;
    return { caption: str(x.caption, 120) ?? "", narration: str(x.narration, 240) ?? "", duration: num(x.duration) ?? 0, frame: x.frame === true, clip: x.clip === true };
  });
  const b = (o.brand && typeof o.brand === "object" ? o.brand : {}) as Record<string, unknown>;
  const colors: Record<string, string> = {};
  const c = (b.colors && typeof b.colors === "object" ? b.colors : {}) as Record<string, unknown>;
  for (const k of ["primary", "secondary", "background", "text"]) if (typeof c[k] === "string" && /^#[0-9a-f]{6}$/i.test(c[k] as string)) colors[k] = c[k] as string;
  const status = o.status === "done" || o.status === "failed" ? o.status : "pending";
  return {
    id,
    status,
    seconds: num(o.seconds),
    stage: str(o.stage, 20),
    title: str(o.title, 100),
    tagline: str(o.tagline, 120),
    style: str(o.style, 20),
    vertical: o.vertical === true,
    motion: str(o.motion, 20),
    duration: num(o.duration),
    bytes: num(o.bytes),
    brand: { name: str(b.name, 80), host: str(b.host, 120), colors },
    stages,
    scenes,
    ...(status === "failed" ? { error: str(o.error, 300) ?? "promo failed" } : {}),
  };
}

const LABELS: Record<string, string> = { brand: "brand", script: "script", frames: "frames", motion: "motion", voice: "voice", music: "music", render: "render" };

/** One line per stage for the chat: `[x] frames: 4/6`. */
export function describePromoProgress(p: Pick<PromoProgress, "stages">): string {
  const mark = (s?: string) => (s === "done" ? "[x]" : s === "running" ? "[~]" : s === "failed" ? "[!]" : s === "skipped" ? "[-]" : "[ ]");
  return STAGES.map((k) => {
    const s = p.stages[k];
    return `${mark(s?.state)} ${LABELS[k]}${s?.detail ? `: ${s.detail}` : ""}`;
  }).join("\n");
}
