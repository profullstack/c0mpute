// MCP tools for the gallery, thin over the same service the API uses:
// gallery_submit, gallery_status, gallery_list.

import { BUILT_WITH, GALLERY_CATEGORIES, galleryResponse } from "../gallery";
import { GALLERY_CONFIG } from "./config";
import { GalleryError, parseSubmission, publicStatus, type GalleryService } from "./service";
import { toEntry } from "./store";
import { normalizeUrl } from "./url";

export const GALLERY_TOOLS = [
  {
    name: "gallery_submit",
    description: `Submit a site built with or hosted on c0mpute to the c0mpute gallery (c0mpute.com/gallery and infernetprotocol.com/gallery). Free tier: listed as soon as the c0mpute badge is found on the site (the result has the HTML snippet), otherwise after a ${GALLERY_CONFIG.freeQueueDays}-day queue. Priority tier: $${GALLERY_CONFIG.priorityPriceUsd} once via CoinPay (the result has a checkout_url for the person to pay), live after automated checks. The URL must answer 200.`,
    inputSchema: {
      type: "object",
      properties: {
        url: { type: "string", description: "The site's public URL (the page to list and check for the badge)." },
        name: { type: "string", description: "Display name (default: the host)." },
        tagline: { type: "string", description: "One sentence: what it is and what runs on c0mpute." },
        category: { type: "string", enum: GALLERY_CATEGORIES.map((c) => c.id) },
        built_with: { type: "array", items: { type: "string" }, description: `c0mpute features used, e.g. ${BUILT_WITH.slice(0, 4).join(", ")}.` },
        contact_email: { type: "string", description: "Where go-live and badge notices go (required for priority)." },
        tier: { type: "string", enum: ["free", "priority"], description: "Default free." },
      },
      required: ["url"],
    },
    annotations: { title: "Submit to the c0mpute gallery", readOnlyHint: false, destructiveHint: false, openWorldHint: true },
  },
  {
    name: "gallery_status",
    description: "A gallery listing's status by its URL or listing id: queued (with the go-live date), waiting for payment, under review, listed or not listed, plus the badge state. Pass check: true to look for the badge now (once a minute).",
    inputSchema: {
      type: "object",
      properties: {
        url: { type: "string", description: "The listed site's URL." },
        id: { type: "string", description: "Or the listing id (gl_...)." },
        check: { type: "boolean", description: "Look for the badge on the site now." },
      },
    },
    annotations: { title: "Gallery listing status", readOnlyHint: false, openWorldHint: true },
  },
  {
    name: "gallery_list",
    description: "Everything listed in the c0mpute gallery (the public feed), optionally one category.",
    inputSchema: { type: "object", properties: { category: { type: "string", enum: GALLERY_CATEGORIES.map((c) => c.id) } } },
    annotations: { title: "List the gallery", readOnlyHint: true, openWorldHint: false },
  },
];
export const GALLERY_TOOL_NAMES = new Set(GALLERY_TOOLS.map((t) => t.name));

export async function galleryTool(name: string, args: Record<string, unknown>, g: GalleryService): Promise<{ text: string; isError?: boolean }> {
  try {
    if (name === "gallery_submit") {
      const input = parseSubmission(args);
      if (typeof input === "string") return { text: input, isError: true };
      const r = await g.submit(input);
      return { text: JSON.stringify({ message: r.message, checkout_url: r.checkout_url, listing: publicStatus(r.listing) }) };
    }
    if (name === "gallery_status") {
      const id = typeof args.id === "string" ? args.id : "";
      const norm = normalizeUrl(args.url);
      let l = id ? await g.store.byId(id) : null;
      if (!l && norm) l = await g.store.byKey(norm.key);
      if (!l) return { text: id || norm ? "not submitted yet: use gallery_submit" : "gallery_status needs url or id", isError: true };
      if (args.check === true) {
        const r = await g.checkNow(l, { manual: true });
        return { text: JSON.stringify({ message: r.message, listing: publicStatus(r.listing) }) };
      }
      return { text: JSON.stringify({ listing: publicStatus(l, await g.store.hits(l.url_key)) }) };
    }
    if (name === "gallery_list") {
      const cat = typeof args.category === "string" ? args.category : null;
      const all = (await g.store.approved()).map(toEntry);
      const r = galleryResponse(cat ? all.filter((e) => e.category === cat) : all);
      return { text: JSON.stringify({ count: r.count, data: r.data.map((e) => ({ name: e.name, url: e.url, category: e.category, tagline: e.tagline, featured: e.featured })) }) };
    }
    return { text: `unknown tool: ${name}`, isError: true };
  } catch (e) {
    if (e instanceof GalleryError) return { text: JSON.stringify({ error: e.message, ...(e.listing ? { listing: publicStatus(e.listing) } : {}) }), isError: true };
    return { text: e instanceof Error ? e.message : String(e), isError: true };
  }
}
