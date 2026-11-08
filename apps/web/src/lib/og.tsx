import { ImageResponse } from "next/og";
import type { Metadata } from "next";

export const SITE = "https://c0mpute.com";
export const OG_SIZE = { width: 1200, height: 630 };

/**
 * Open Graph for one page. Next replaces the layout's `openGraph` object
 * wholesale when a page sets its own, so every page that wants its own title
 * goes through here to keep type/site name. The image comes from the nearest
 * `opengraph-image.tsx`, never from a static file (the old /og-image.png 404ed
 * and left every social preview blank).
 */
export function pageOg(title: string, description: string, path: string): Pick<Metadata, "openGraph" | "twitter"> {
  return {
    openGraph: { type: "website", siteName: "c0mpute", title, description, url: `${SITE}${path}` },
    twitter: { card: "summary_large_image", title, description },
  };
}

/** The c0mpute card: dark terminal, green accent, a kicker, a title, a footer line. */
export function ogCard({ kicker, title, sub }: { kicker: string; title: string; sub: string }) {
  return new ImageResponse(
    (
      <div
        style={{
          width: "100%",
          height: "100%",
          display: "flex",
          flexDirection: "column",
          justifyContent: "space-between",
          padding: "64px 72px",
          background: "#0a0a0b",
          color: "#d4d4d4",
          fontFamily: "monospace",
          borderTop: "10px solid #4ade80",
        }}
      >
        <div style={{ display: "flex", fontSize: 40, fontWeight: 700, color: "#4ade80" }}>c0mpute</div>
        <div style={{ display: "flex", flexDirection: "column", gap: 24 }}>
          <div style={{ display: "flex", fontSize: 30, color: "#6b7280" }}>{`// ${kicker}`}</div>
          <div style={{ display: "flex", fontSize: title.length > 40 ? 64 : 76, fontWeight: 700, lineHeight: 1.1, color: "#f5f5f5" }}>
            {title}
          </div>
        </div>
        <div style={{ display: "flex", justifyContent: "space-between", fontSize: 28, color: "#6b7280" }}>
          <span>{sub}</span>
          <span style={{ color: "#4ade80" }}>c0mpute.com</span>
        </div>
      </div>
    ),
    OG_SIZE,
  );
}
