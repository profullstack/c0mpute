// The "Listed on the c0mpute gallery" badge: one SVG per theme, keyed by the
// listing's URL only (?url=), never a product id. The only thing recorded is a
// hit count per URL, buffered in memory and flushed to the database once a
// minute: no cookies, no IPs, no per-visitor anything.

import { GALLERY_ORIGIN } from "./config";
import { normalizeUrl } from "./url";

export const BADGE_WIDTH = 170;
export const BADGE_HEIGHT = 36;
export const BADGE_ALT = "Listed on the c0mpute gallery";

export type BadgeTheme = "light" | "dark";

export function badgeTheme(v: string | null | undefined): BadgeTheme {
  return v === "dark" ? "dark" : "light";
}

/** The badge itself: static per theme, so any cache can hold it. */
export function badgeSvg(theme: BadgeTheme): string {
  const c =
    theme === "dark"
      ? { bg: "#0a0a0b", border: "#2a2f37", text: "#d4d4d4", dim: "#8b929c", accent: "#4ade80" }
      : { bg: "#ffffff", border: "#d0d5dc", text: "#111114", dim: "#5b616b", accent: "#15803d" };
  const w = BADGE_WIDTH;
  const h = BADGE_HEIGHT;
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" role="img" aria-label="${BADGE_ALT}">
<title>${BADGE_ALT}</title>
<rect x="0.5" y="0.5" width="${w - 1}" height="${h - 1}" rx="7" fill="${c.bg}" stroke="${c.border}"/>
<g font-family="ui-monospace,SFMono-Regular,Menlo,Consolas,'Liberation Mono',monospace">
<text x="12" y="23" font-size="15" font-weight="700" fill="${c.accent}">&gt;_</text>
<text x="36" y="15" font-size="9" fill="${c.dim}" letter-spacing=".4">LISTED ON THE</text>
<text x="36" y="28" font-size="13" font-weight="700" fill="${c.text}">c0mpute gallery</text>
</g>
</svg>`;
}

/**
 * The URL a badge request is for: ?url= when given, else the page that
 * embedded it (Referer; browsers send at least the origin cross-site).
 * Returns the normalized key, or null.
 */
export function badgeKey(urlParam: string | null, referer: string | null): string | null {
  const fromParam = urlParam ? normalizeUrl(urlParam) : null;
  if (fromParam) return fromParam.key;
  if (!referer) return null;
  try {
    const r = new URL(referer);
    if (r.hostname === "c0mpute.com" || r.hostname.endsWith(".c0mpute.com")) return null;
    return normalizeUrl(`${r.protocol}//${r.host}${r.pathname}`)?.key ?? null;
  } catch {
    return null;
  }
}

/** Copy-paste HTML for a site: the badge linking back to the gallery. */
export function badgeSnippet(url: string, theme: BadgeTheme = "light"): string {
  const u = encodeURIComponent(url);
  const t = theme === "dark" ? "&amp;theme=dark" : "";
  return `<a href="${GALLERY_ORIGIN}/gallery?ref=${u}"><img src="${GALLERY_ORIGIN}/badge.svg?url=${u}${t}" alt="${BADGE_ALT}" width="${BADGE_WIDTH}" height="${BADGE_HEIGHT}"></a>`;
}

/** Markdown for a README. */
export function badgeMarkdown(url: string): string {
  const u = encodeURIComponent(url);
  return `[![${BADGE_ALT}](${GALLERY_ORIGIN}/badge.svg?url=${u})](${GALLERY_ORIGIN}/gallery?ref=${u})`;
}

// ── hit counts ───────────────────────────────────────────────────────────────
const pending = new Map<string, number>();
const MAX_KEYS = 10_000;

/** Count one badge load for this URL key (flushed by flushHits). */
export function countHit(key: string): void {
  if (!pending.has(key) && pending.size >= MAX_KEYS) return;
  pending.set(key, (pending.get(key) ?? 0) + 1);
}

/** Take the buffered counts (and clear them). */
export function takeHits(): Map<string, number> {
  const out = new Map(pending);
  pending.clear();
  return out;
}
