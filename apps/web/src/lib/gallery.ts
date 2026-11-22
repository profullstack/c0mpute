/**
 * The c0mpute gallery: things built with, or hosted on, the c0mpute network.
 *
 * Listings live in the site database (lib/gallery/store.ts, table
 * gallery_listings) and are what c0mpute.com/gallery and the public
 * GET /api/v1/gallery (read by infernetprotocol.com/gallery) show. GALLERY
 * below is the editorial seed: written into the table as approved on first
 * start, and served as-is if the database cannot be read. People add their own
 * through /gallery/submit (free with the badge or after the queue, or priority).
 *
 * The response shape is documented in docs/gallery-api.md and is versioned: add
 * fields freely, never rename or remove one without bumping GALLERY_SCHEMA_VERSION.
 *
 * Rules for a seed entry: it must answer 200 at `url` when added, the c0mpute
 * part must be true (verified from code, receipts or deploy notes), and no
 * invented numbers or users. Seed screenshots live in public/gallery/<slug>.png;
 * submitted listings get a generated card at /gallery/card/<slug>.
 */

export const GALLERY_SCHEMA_VERSION = 1;

export const GALLERY_CATEGORIES = [
  { id: "apps", label: "apps" },
  { id: "automation", label: "automation" },
  { id: "media", label: "music & audio" },
  { id: "models", label: "fine-tuned models" },
] as const;

export type GalleryCategory = (typeof GALLERY_CATEGORIES)[number]["id"];

export type GalleryEntry = {
  /** Stable id, lowercase kebab-case. Never changes once published. */
  slug: string;
  name: string;
  /** One sentence. */
  tagline: string;
  /** The live site (canonical home). */
  url: string;
  /** The copy served from c0mpute hosting (a gateway URL), or null when the thing runs on c0mpute but is not hosted there. */
  c0mpute_url: string | null;
  category: GalleryCategory;
  /** c0mpute features used, from a fixed-ish vocabulary: "c0mpute hosting", "c0mpute chat API", "Infernet chat", "fine-tune", "c0mpute speech", "c0mpute music", "c0mpute jobs", "model router". */
  built_with: string[];
  /** Site-relative path to the screenshot; the API turns it into an absolute URL. */
  image: string;
  /** Date it went live on c0mpute (YYYY-MM-DD). */
  added_at: string;
  /** Picked out by the editors (shown first on c0mpute.com/gallery). */
  featured?: boolean;
  /** How it got listed: "editorial" (seed), "badge", "queue", "priority" or "admin". */
  listed_via?: ListedVia;
};

export type ListedVia = "editorial" | "badge" | "queue" | "priority" | "admin";

export const GALLERY: GalleryEntry[] = [
  {
    slug: "board-saas",
    name: "Board as a Service",
    tagline:
      "Paste a decision brief: five AI advisors and a chair debate it and write the minutes. The meetings run on c0mpute models, and the front page is hosted on c0mpute. First meeting free, then $9 via CoinPay.",
    url: "https://board.saas.pit.world",
    c0mpute_url: "https://gateway.c0mpute.com/s/d28960255346f386e99c/",
    category: "apps",
    built_with: ["c0mpute chat API", "c0mpute hosting"],
    image: "/gallery/board-saas.png",
    added_at: "2026-11-22",
  },
  {
    slug: "nixamp-music",
    name: "nixamp music",
    tagline:
      "AI bands and albums, live stations and on-demand listening. Songs are made with ACE-Step on c0mpute, and the music library is mirrored from a c0mpute hosting site.",
    url: "https://nixamp.com/music",
    c0mpute_url: "https://gateway.c0mpute.com/s/28c1945dfaf4e11f1350/",
    category: "media",
    built_with: ["c0mpute music", "c0mpute hosting"],
    image: "/gallery/nixamp-music.png",
    added_at: "2026-11-11",
  },
  {
    slug: "tui-weekly",
    name: "TUI Weekly podcast",
    tagline:
      "A weekly podcast about terminal user interfaces, hosted by Profullstack's AI agents. Every voice is rendered on c0mpute speech nodes.",
    url: "https://tuiweekly.bl0ggers.com/episodes",
    c0mpute_url: null,
    category: "media",
    built_with: ["c0mpute speech"],
    image: "/gallery/tui-weekly.png",
    added_at: "2026-11-11",
  },
  {
    slug: "meshhook",
    name: "MeshHook",
    tagline:
      "A webhook-first workflow engine, like a peer-to-peer n8n. AI steps run on the open model market and data steps run as c0mpute jobs.",
    url: "https://meshhook.com",
    c0mpute_url: null,
    category: "automation",
    built_with: ["Infernet chat", "c0mpute jobs"],
    image: "/gallery/meshhook.png",
    added_at: "2026-11-02",
  },
  {
    slug: "bot-seo",
    name: "bot.seo",
    tagline:
      "SEO on autopilot for every domain you own: paste your domains and it audits them, plans keywords and writes articles with models on c0mpute.",
    url: "https://bot.seo.w3bs.org",
    c0mpute_url: null,
    category: "automation",
    built_with: ["Infernet chat"],
    image: "/gallery/bot-seo.png",
    added_at: "2026-11-02",
  },
  {
    slug: "c0mpute-chat",
    name: "c0mpute chat",
    tagline:
      "Chat with open models served by nodes on the network, from the web, the CLI, an OpenAI-compatible API or MCP.",
    url: "https://c0mpute.com/chat",
    c0mpute_url: null,
    category: "apps",
    built_with: ["c0mpute chat API", "model router"],
    image: "/gallery/c0mpute-chat.png",
    added_at: "2026-11-02",
  },
  {
    slug: "not-a-lawyer-agent",
    name: "not-a-lawyer-agent",
    tagline:
      "A small (0.5B) legal-research model fine-tuned on the c0mpute network from public legal writing. Experimental, and not legal advice.",
    url: "https://huggingface.co/profullstack/not-a-lawyer-agent-GGUF",
    c0mpute_url: null,
    category: "models",
    built_with: ["fine-tune"],
    image: "/gallery/not-a-lawyer-agent.png",
    added_at: "2026-11-02",
  },
  {
    slug: "tax-guy-agent",
    name: "tax-guy-agent",
    tagline:
      "A small (0.5B) tax-research model fine-tuned on the c0mpute network from public tax writing. Experimental, and not tax advice.",
    url: "https://huggingface.co/profullstack/tax-guy-agent-GGUF",
    c0mpute_url: null,
    category: "models",
    built_with: ["fine-tune"],
    image: "/gallery/tax-guy-agent.png",
    added_at: "2026-11-02",
  },
];

/** The public JSON shape served at /api/v1/gallery. */
export function galleryResponse(entries: GalleryEntry[] = GALLERY, origin = "https://c0mpute.com") {
  const data = entries.map((e) => ({
    ...e,
    image: new URL(e.image, origin).toString(),
    featured: e.featured ?? false,
    listed_via: e.listed_via ?? "editorial",
  }));
  return {
    object: "list" as const,
    schema_version: GALLERY_SCHEMA_VERSION,
    updated_at: entries.reduce((a, e) => (e.added_at > a ? e.added_at : a), ""),
    count: data.length,
    categories: GALLERY_CATEGORIES.map((c) => ({ ...c, count: entries.filter((e) => e.category === c.id).length })),
    submit_url: SUBMIT_URL,
    data,
  };
}

/** Where people get listed ("Get listed": free with the badge, or priority). */
export const SUBMIT_URL = "https://c0mpute.com/gallery/submit";

/** c0mpute features a submitter can tick; the field stays an open vocabulary. */
export const BUILT_WITH = [
  "c0mpute hosting",
  "c0mpute chat API",
  "Infernet chat",
  "c0mpute jobs",
  "c0mpute music",
  "c0mpute speech",
  "c0mpute images",
  "c0mpute video",
  "fine-tune",
  "model router",
  "c0mpute apps",
] as const;
