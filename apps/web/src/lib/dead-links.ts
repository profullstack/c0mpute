// Posts arrive from the Crawlproof autoblog webhook with absolute URLs baked
// into their HTML. Some of those targets are gone for good (ThreatCrush Link
// Watch, 2026-10-17): the Supabase project that hosted article images no longer
// resolves, and one c0upons.com post 404s. Rows are imported data, so they are
// cleaned on read instead of being edited.

const DEAD_IMAGE_HOSTS = ["ywcizjsgrcmhgyplldac.supabase.co"];
const DEAD_LINK_PREFIXES = ["https://c0upons.com/blog/walgreens-coupon-code-workflow"];

export function isDeadImageUrl(url: string | null | undefined): boolean {
  if (!url) return false;
  try {
    return DEAD_IMAGE_HOSTS.includes(new URL(url).hostname);
  } catch {
    return false;
  }
}

/** Drops images on dead hosts and unlinks (keeping the text) known-dead anchors. */
export function stripDeadLinks(html: string): string {
  let out = html;
  if (DEAD_IMAGE_HOSTS.some((h) => out.includes(h))) {
    // <figure> wrapping a dead image goes whole; bare <img> otherwise.
    out = out.replace(/<figure\b[^>]*>(?:(?!<\/figure>)[\s\S])*?<\/figure>/gi, (m) =>
      DEAD_IMAGE_HOSTS.some((h) => m.includes(h)) ? "" : m,
    );
    out = out.replace(/<img\b[^>]*>/gi, (m) =>
      DEAD_IMAGE_HOSTS.some((h) => m.includes(h)) ? "" : m,
    );
  }
  if (DEAD_LINK_PREFIXES.some((p) => out.includes(p))) {
    out = out.replace(/<a\b[^>]*>([\s\S]*?)<\/a>/gi, (m, inner) =>
      DEAD_LINK_PREFIXES.some((p) => m.includes(`href="${p}`) || m.includes(`href='${p}`)) ? inner : m,
    );
  }
  return out;
}
