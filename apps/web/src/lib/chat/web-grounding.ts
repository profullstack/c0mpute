// Web grounding for c0mpute.com/chat (2026-11-22). A small open model answering
// "what does a 2BR in the Marina rent for?" from memory invents numbers and
// sources. Before the model runs, a factual or current question is searched on
// pit.world (our own SearXNG + Linkup + pit index), the top pages are read for
// their photo (og:image), title, description and the sentences that answer the
// question, and the model is told to answer only from those numbered sources.
// The same sources go to the browser as cards (photo, caption, link).
//
// Never blocks an answer: every step has a hard budget, and any failure (search
// down, rate limited, pages refusing bots) answers without sources.

import { lookup } from "node:dns/promises";
import { isIP } from "node:net";
import { C0_SYSTEM_PROMPT, type ChatMessage } from "./completions";
import { isBlockedAddress, isPublicHostname } from "./plugin-net";

export type WebSource = {
  n: number;
  title: string;
  url: string;
  host: string;
  image: string | null;
  snippet: string;
};

export type Grounding = { query: string; sources: WebSource[]; context: string };

/** The whole grounding step (search + reading pages) never takes longer than this. */
// A cold pit.world answers in ~5 s (warm: ~1 s); 4.5 s dropped those searches.
export const GROUNDING_BUDGET_MS = 9_500;
const SEARCH_MS = 6_500;
const PAGE_MS = 3_500;
const PAGE_MAX_BYTES = 700_000;
const PAGES_READ = 5;
const SOURCES_KEPT = 5;
const EXCERPT_CHARS = 900;

const SEARCH_URL = process.env.C0MPUTE_SEARCH_URL || "https://pit.world/api/search";

/**
 * Worth a web search: a question about the world (prices, places, people,
 * news, comparisons), not code, creative writing or editing text the person
 * pasted. Errs toward searching: a needless search costs a second or two, a
 * missing one costs a made-up answer.
 */
export function needsWeb(text: string): boolean {
  const t = text.trim();
  if (t.length < 8 || t.length > 1_500) return false;
  if (t.startsWith("/")) return false;
  if (/```|^\s*(import|def|function|class|const|let|var|#include)\b/m.test(t)) return false;
  const lower = t.toLowerCase();
  // Making or reworking text, not looking something up.
  if (/^(write|compose|draft|rewrite|rephrase|translate|summari[sz]e (this|the following)|fix (this|my)|refactor|explain (this|the following) code|generate (a|an) (poem|story|song|joke))\b/.test(lower)) return false;
  if (/\b(poem|haiku|short story|limerick|lyrics)\b/.test(lower) && !/\?/.test(lower)) return false;
  const asks = /\?|^(what|whats|what's|where|when|who|whom|which|why|how|is|are|can|could|should|does|do|did|will|tell me|give me|find|show me|list|compare|recommend|best)\b|\b(tell me|give me|how much|how many|look up|find me)\b/.test(lower);
  const worldly = /\b(price|prices|cost|costs|rent|rents|average|median|salary|best|top|near|nearby|neighborhood|neighbourhood|city|town|restaurant|hotel|apartment|house|weather|news|latest|today|tonight|this week|current|currently|202\d|open now|hours|review|reviews|vs\.?|versus|compare|where to|how to get|schedule|ferry|flight|ticket|stock|market|population|law|election)\b/.test(lower);
  return asks || worldly;
}

/** The search query: the person's question, trimmed of chat filler. */
export function searchQuery(text: string): string {
  return text
    .replace(/\s+/g, " ")
    .replace(/^(hey|hi|hello|ok|okay|so|please|pls)[,!. ]+/i, "")
    .replace(/^(can|could|would) (you|u) (please )?(tell me|give me|find|show me|look up)\s*/i, "")
    .trim()
    .slice(0, 200);
}

type Hit = { url: string; title: string; description: string; host: string };

const cache = new Map<string, { at: number; hits: Hit[] }>();
const CACHE_MS = 10 * 60_000;

async function search(query: string, signal: AbortSignal, fetchImpl: typeof fetch): Promise<Hit[]> {
  const key = query.toLowerCase();
  const hit = cache.get(key);
  if (hit && Date.now() - hit.at < CACHE_MS) return hit.hits;
  const u = new URL(SEARCH_URL);
  u.searchParams.set("q", query);
  u.searchParams.set("n", "10");
  const res = await fetchImpl(u, { signal: AbortSignal.any([signal, AbortSignal.timeout(SEARCH_MS)]), headers: { accept: "application/json", "user-agent": "c0mpute-chat/1 (+https://c0mpute.com/chat)" } });
  if (!res.ok) throw new Error(`search HTTP ${res.status}`);
  const body = (await res.json()) as { results?: { url?: unknown; title?: unknown; description?: unknown; host?: unknown }[] };
  const hits: Hit[] = [];
  for (const r of body.results ?? []) {
    if (typeof r.url !== "string" || !/^https?:\/\//.test(r.url)) continue;
    let host = typeof r.host === "string" ? r.host : "";
    try {
      host ||= new URL(r.url).hostname;
    } catch {
      continue;
    }
    hits.push({ url: r.url, title: clean(String(r.title ?? host)).slice(0, 160), description: clean(String(r.description ?? "")).slice(0, 400), host: host.replace(/^www\./, "") });
  }
  if (cache.size > 500) cache.clear();
  cache.set(key, { at: Date.now(), hits });
  return hits;
}

/** https only, a public hostname that resolves only to public addresses. */
async function safeUrl(raw: string): Promise<URL | null> {
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    return null;
  }
  if (u.protocol !== "https:" || u.username || u.password) return null;
  if (u.port && u.port !== "443") return null;
  const host = u.hostname.replace(/^\[|\]$/g, "");
  if (isIP(host)) return isBlockedAddress(host) ? null : u;
  if (!isPublicHostname(host)) return null;
  try {
    const addrs = await lookup(host, { all: true });
    if (!addrs.length || addrs.some((a) => isBlockedAddress(a.address))) return null;
  } catch {
    return null;
  }
  return u;
}

type Page = { image: string | null; title: string | null; description: string | null; text: string };

/** Fetch one page (no redirects to unchecked hosts) and read what grounding needs from it. */
async function readPage(raw: string, signal: AbortSignal, fetchImpl: typeof fetch): Promise<Page | null> {
  let url = await safeUrl(raw);
  for (let hop = 0; url && hop < 3; hop++) {
    const res = await fetchImpl(url, {
      redirect: "manual",
      signal: AbortSignal.any([signal, AbortSignal.timeout(PAGE_MS)]),
      headers: { accept: "text/html,application/xhtml+xml", "user-agent": "Mozilla/5.0 (compatible; c0mpute-chat/1; +https://c0mpute.com/chat)", "accept-language": "en-US,en;q=0.8" },
    });
    if (res.status >= 300 && res.status < 400) {
      const next = res.headers.get("location");
      await res.body?.cancel().catch(() => {});
      url = next ? await safeUrl(new URL(next, url).href) : null;
      continue;
    }
    if (!res.ok || !(res.headers.get("content-type") ?? "").includes("html")) {
      await res.body?.cancel().catch(() => {});
      return null;
    }
    const html = await readCapped(res, PAGE_MAX_BYTES);
    return parsePage(html, url);
  }
  return null;
}

async function readCapped(res: Response, max: number): Promise<string> {
  const reader = res.body?.getReader();
  if (!reader) return "";
  const parts: Uint8Array[] = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    parts.push(value);
    size += value.byteLength;
    if (size >= max) {
      await reader.cancel().catch(() => {});
      break;
    }
  }
  return new TextDecoder().decode(Buffer.concat(parts));
}

const ENTITIES: Record<string, string> = { amp: "&", lt: "<", gt: ">", quot: '"', apos: "'", nbsp: " ", "#39": "'", mdash: "-", ndash: "-", hellip: "...", rsquo: "'", lsquo: "'", rdquo: '"', ldquo: '"' };

function clean(s: string): string {
  return s
    .replace(/&(#x?[0-9a-f]+|[a-z]+\d*);/gi, (m, e: string) => {
      const k = e.toLowerCase();
      if (ENTITIES[k]) return ENTITIES[k]!;
      if (k.startsWith("#x")) return String.fromCodePoint(parseInt(k.slice(2), 16) || 32);
      if (k.startsWith("#")) return String.fromCodePoint(parseInt(k.slice(1), 10) || 32);
      return m;
    })
    .replace(/\s+/g, " ")
    .trim();
}

function meta(html: string, names: string[]): string | null {
  for (const name of names) {
    const a = new RegExp(`<meta[^>]+(?:property|name)=["']${name}["'][^>]*content=["']([^"']+)["']`, "i").exec(html);
    const b = new RegExp(`<meta[^>]+content=["']([^"']+)["'][^>]*(?:property|name)=["']${name}["']`, "i").exec(html);
    const v = a?.[1] ?? b?.[1];
    if (v) return clean(v);
  }
  return null;
}

export function parsePage(html: string, base: URL): Page {
  const imageRaw = meta(html, ["og:image:secure_url", "og:image", "twitter:image", "twitter:image:src"]);
  let image: string | null = null;
  if (imageRaw) {
    try {
      const u = new URL(imageRaw, base);
      if (u.protocol === "https:" && !/\.svg(\?|$)/i.test(u.pathname)) image = u.href;
    } catch {}
  }
  const title = meta(html, ["og:title", "twitter:title"]) ?? (/<title[^>]*>([^<]{1,300})<\/title>/i.exec(html)?.[1] ? clean(/<title[^>]*>([^<]{1,300})<\/title>/i.exec(html)![1]!) : null);
  const description = meta(html, ["og:description", "description", "twitter:description"]);
  const text = clean(
    html
      .replace(/<(script|style|noscript|svg|nav|footer|header|form|iframe|template)[\s\S]*?<\/\1>/gi, " ")
      .replace(/<!--[\s\S]*?-->/g, " ")
      .replace(/<\/(p|div|li|h[1-6]|tr|td|th|section|article|br)>/gi, ". ")
      .replace(/<[^>]+>/g, " "),
  );
  return { image, title, description, text };
}

const STOP = new Set("a an and are as at be by can could do for from give have how i in is it me my of on or so tell than that the then this to too us was what whats when where which who why will with you your".split(" "));

function terms(q: string): string[] {
  return [...new Set(q.toLowerCase().replace(/[^a-z0-9$ ]+/g, " ").split(" ").filter((w) => w.length > 2 && !STOP.has(w)))];
}

/** The sentences of a page that answer the question: query words, numbers and prices first, kept in page order. */
export function excerpt(text: string, query: string, max = EXCERPT_CHARS): string {
  const words = terms(query);
  const sentences = text.split(/(?<=[.!?])\s+/).map((s) => s.trim()).filter((s) => s.length > 30 && s.length < 400);
  const scored = sentences.map((s, i) => {
    const l = s.toLowerCase();
    let score = 0;
    for (const w of words) if (l.includes(w)) score += 2;
    if (/\$\s?\d|\d[\d,]*\s?(\/mo|per month|a month|month)/i.test(s)) score += 3;
    else if (/\d/.test(s)) score += 1;
    return { s, i, score };
  });
  const picked: { s: string; i: number }[] = [];
  let used = 0;
  for (const c of [...scored].sort((a, b) => b.score - a.score)) {
    if (c.score <= 0 || used + c.s.length > max) continue;
    picked.push(c);
    used += c.s.length + 1;
  }
  return picked
    .sort((a, b) => a.i - b.i)
    .map((c) => c.s)
    .join(" ");
}

/**
 * Search and read the web for a question. Null when nothing useful came back
 * (or anything failed) inside the budget: the turn then answers without sources.
 */
export async function groundQuestion(
  question: string,
  opts: { signal?: AbortSignal; fetch?: typeof fetch; budgetMs?: number; onStatus?: (message: string) => void } = {},
): Promise<Grounding | null> {
  const fetchImpl = opts.fetch ?? fetch;
  const budget = AbortSignal.timeout(opts.budgetMs ?? GROUNDING_BUDGET_MS);
  const signal = opts.signal ? AbortSignal.any([opts.signal, budget]) : budget;
  const query = searchQuery(question);
  if (!query) return null;
  const plan = searchPlan(question);
  opts.onStatus?.(plan.queries.length > 1 ? `searching pit.world (${plan.queries.length} searches)…` : "searching pit.world…");
  // Web searches and place summaries side by side; any of them may fail alone.
  const [lists, places] = await Promise.all([
    Promise.all(
      plan.queries.map((q) =>
        search(q, signal, fetchImpl).catch((e) => {
          console.warn("chat grounding: search failed", e instanceof Error ? e.message : e);
          return [] as Hit[];
        }),
      ),
    ),
    Promise.all(plan.subjects.slice(0, 3).map((s) => wikiPlace(s, signal, fetchImpl).catch(() => null))),
  ]);
  // Round-robin across the searches (every part of the question gets sources),
  // one result per site within each search, no URL twice.
  const perList = lists.map((hits) => {
    const seen = new Set<string>();
    return hits.filter((h) => relevantHit(question, h.host) && !seen.has(h.host) && seen.add(h.host));
  });
  const top: { hit: Hit; q: string }[] = [];
  const urls = new Set<string>();
  const keep = plan.queries.length > 1 ? SOURCES_KEPT + 1 : SOURCES_KEPT;
  for (let rank = 0; top.length < keep && perList.some((l) => l.length > rank); rank++) {
    perList.forEach((l, qi) => {
      const h = l[rank];
      if (h && top.length < keep && !urls.has(h.url)) {
        urls.add(h.url);
        top.push({ hit: h, q: plan.queries[qi]! });
      }
    });
  }
  const placeList = places.filter((p): p is WikiPlace => !!p && !urls.has(p.url));
  if (!top.length && !placeList.length) return null;
  if (top.length) opts.onStatus?.(`reading ${Math.min(PAGES_READ, top.length)} pages…`);
  const pages = await Promise.all(top.map((t, i) => (i < PAGES_READ ? readPage(t.hit.url, signal, fetchImpl).catch(() => null) : Promise.resolve(null))));
  const sources: WebSource[] = [];
  const blocks: string[] = [];
  top.forEach(({ hit: h, q }, i) => {
    const p = pages[i];
    const n = sources.length + 1;
    const body = (p?.text ? excerpt(p.text, q) : "") || p?.description || h.description;
    if (!body && !h.description) return;
    // A search title that is just the site name ("Apartmentlist") loses to the page's own title.
    const title = (h.title && h.title.length > 20 ? h.title : p?.title || h.title) || h.host;
    sources.push({ n, title, url: h.url, host: h.host, image: p?.image ?? null, snippet: (p?.description || h.description || body).slice(0, 240) });
    blocks.push(`[${n}] ${title} (${h.host})\nURL: ${h.url}\n${h.description ? `Search snippet: ${h.description}\n` : ""}${body && body !== h.description ? `From the page: ${body}` : ""}`.trim());
  });
  for (const pl of placeList) {
    const n = sources.length + 1;
    sources.push({ n, title: pl.title, url: pl.url, host: "wikipedia.org", image: pl.image, snippet: (pl.description || pl.extract).slice(0, 240) });
    blocks.push(`[${n}] ${pl.title} (wikipedia.org: about the place)\nURL: ${pl.url}\n${pl.extract}`);
  }
  if (!sources.length) return null;
  return { query, sources, context: blocks.join("\n\n") };
}

// ── splitting a question into searches ───────────────────────────────────

/** Words that describe what is wanted (price, size, amenities), not where or who. */
const GENERIC = new Set(
  (
    "average avg avareg avarage median typical cost costs price prices pricing rent rents rental rentals monthly month per much many " +
    "decent halfway good nice cheap affordable expensive best top great okay ok quality " +
    "br 1br 2br 3br 4br bed beds bedroom bedrooms bath baths bathroom studio apartment apartments apt apts flat house houses home homes condo unit units place places " +
    "off street offstreet parking parkig garage laundry washer dryer dishwasher dishwashers amenities in-unit unit " +
    "district neighborhood neighbourhood area areas part side city town " +
    "give tell show find get want need looking look compare comparison versus info information about also too then like " +
    "canyou can't cant pls please hey hi thanks"
  ).split(" "),
);

/** Generic words that only judge (not what to search for in a follow-up clause). */
const VAGUE = new Set("decent halfway good nice okay ok quality great best top give tell show find get want need looking look about also too then like canyou cant can't pls please hey hi thanks off street".split(" "));

/** The place or thing a clause is about: its words minus stop words and generic wants. */
export function clauseSubject(clause: string): string | null {
  const words = clause
    .toLowerCase()
    .replace(/[^a-z0-9']+/g, " ")
    .split(/\s+/)
    .filter((w) => w && !STOP.has(w) && !GENERIC.has(w) && !/^\d+$/.test(w));
  if (!words.length || words.length > 5) return null;
  return words.join(" ");
}

/**
 * The searches a question needs: the whole question, plus one per later clause
 * that names a new subject ("... in the Marina? and then give me Inner Sunset
 * too" also searches "inner sunset" with the first clause's wants), and the
 * subjects worth a place summary.
 */
export function searchPlan(question: string): { queries: string[]; subjects: string[] } {
  const main = searchQuery(question);
  const clauses = question
    .split(/\?|;|\band then\b|\bthen\b|\balso\b|\bwhat about\b|\bhow about\b|\bversus\b|\bvs\.?(?=\s)|\bcompared? (?:to|with)\b/i)
    .map((c) => c.trim())
    .filter((c) => terms(c).length);
  const first = clauses[0] ?? main;
  const wants = terms(first.replace(/-/g, " ")).filter((w) => GENERIC.has(w) && !VAGUE.has(w)).slice(0, 6);
  // The city the question is in carries to the follow-up searches ("inner
  // sunset" alone found a "Sunset 2BR" in Plovdiv, 2026-11-22).
  const cityMatch = CITY_RE.exec(question);
  const city = cityMatch ? (CITY_ALIAS[cityMatch[0].toLowerCase()] ?? cityMatch[0]) : "";
  const subjects: string[] = [];
  const queries = [main];
  clauses.forEach((c, i) => {
    // A long first clause usually ends on its place ("... on SF marina district").
    const s = (i === 0 ? firstSubject(c) : null) ?? clauseSubject(c);
    if (!s || subjects.includes(s)) return;
    subjects.push(s);
    if (i > 0) {
      const inCity = city && !s.toLowerCase().includes(city.toLowerCase()) ? ` ${city}` : "";
      queries.push(`${s}${inCity} ${wants.join(" ")}`.replace(/\s+/g, " ").trim().slice(0, 200));
    }
  });
  return { queries: queries.slice(0, 3), subjects };
}

const CITY_ALIAS: Record<string, string> = { sf: "San Francisco", nyc: "New York", la: "Los Angeles", dc: "Washington DC", philly: "Philadelphia", atx: "Austin" };
const CITY_RE =
  /\b(sf|nyc|la|dc|philly|atx|san francisco|oakland|berkeley|san jose|sausalito|los angeles|san diego|seattle|portland|new york|brooklyn|boston|chicago|austin|denver|miami|washington dc|philadelphia|atlanta|london|paris|berlin|toronto|vancouver)\b/i;

/** Vacation and hotel sites: wrong for "what does an apartment rent for". */
const VACATION_HOSTS = /(^|\.)(booking\.com|airbnb\.[a-z.]+|vrbo\.com|expedia\.[a-z.]+|tripadvisor\.[a-z.]+|hotels\.com|agoda\.com|trivago\.[a-z.]+|kayak\.[a-z.]+)$/i;
const HOUSING_Q = /\b(rent|rents|rental|lease|apartment|apartments|apt|br|[1-4]br|bedroom|studio|condo|housing|landlord)\b/i;

/** Drop results that cannot answer the question (vacation sites for a housing question). */
export function relevantHit(question: string, host: string): boolean {
  return !(HOUSING_Q.test(question) && VACATION_HOSTS.test(host));
}

/** A long first clause: the trailing place phrase ("... on SF marina district" → "sf marina district"). */
function firstSubject(clause: string): string | null {
  const m = /\b(?:in|on|at|near|around|for)\s+(?:the\s+)?([a-z0-9' -]{3,60})$/i.exec(clause.trim());
  return m ? clauseSubject(m[1]!) ?? m[1]!.toLowerCase().trim().split(/\s+/).slice(0, 4).join(" ") : null;
}

// ── place summaries (Wikipedia) ──────────────────────────────────────────

type WikiPlace = { title: string; url: string; description: string; extract: string; image: string | null };

const WIKI_MS = 3_500;
const WIKI_HEADERS = { accept: "application/json", "user-agent": "c0mpute-chat/1 (+https://c0mpute.com/chat)" };

/** The Wikipedia page a subject names: its summary and lead photo, when the title really matches the subject. */
async function wikiPlace(subject: string, signal: AbortSignal, fetchImpl: typeof fetch): Promise<WikiPlace | null> {
  const s = AbortSignal.any([signal, AbortSignal.timeout(WIKI_MS)]);
  const q = subject.replace(/\bsf\b/i, "San Francisco");
  const found = await fetchImpl(`https://en.wikipedia.org/w/rest.php/v1/search/page?q=${encodeURIComponent(q)}&limit=3`, { signal: s, headers: WIKI_HEADERS });
  if (!found.ok) return null;
  const pages = ((await found.json()) as { pages?: { key?: string; title?: string; description?: string | null }[] }).pages ?? [];
  const want = terms(q);
  const page = pages.find((p) => {
    const t = (p.title ?? "").toLowerCase();
    const hits = want.filter((w) => t.includes(w)).length;
    return p.key && hits >= Math.max(1, Math.ceil(want.length / 2)) && !/disambiguation/i.test(p.description ?? "");
  });
  if (!page?.key) return null;
  const sum = await fetchImpl(`https://en.wikipedia.org/api/rest_v1/page/summary/${encodeURIComponent(page.key)}`, { signal: s, headers: WIKI_HEADERS });
  if (!sum.ok) return null;
  const j = (await sum.json()) as { type?: string; title?: string; description?: string; extract?: string; thumbnail?: { source?: string }; originalimage?: { source?: string }; content_urls?: { desktop?: { page?: string } } };
  if (j.type === "disambiguation" || !j.extract) return null;
  const img = j.thumbnail?.source ?? j.originalimage?.source ?? null;
  return {
    title: j.title ?? page.title ?? subject,
    url: j.content_urls?.desktop?.page ?? `https://en.wikipedia.org/wiki/${encodeURIComponent(page.key)}`,
    description: j.description ?? "",
    extract: j.extract.slice(0, 700),
    image: img && img.startsWith("https://") ? img : null,
  };
}

/** The system message that makes a small model answer like a well-read local, from the sources only. */
export function groundingSystemMessage(g: Grounding, today = new Date()): ChatMessage {
  const date = today.toISOString().slice(0, 10);
  return {
    role: "system",
    content: [
      C0_SYSTEM_PROMPT,
      "",
      `Today is ${date}. The app searched the web for the user's question and read these pages just now:`,
      "",
      g.context,
      "",
      "How to answer:",
      "- Lead with the direct answer in one or two sentences, then go deeper.",
      "- Use facts, prices and numbers ONLY from the sources above, and cite them inline like [1] or [2][4]. Never invent a number, a statistic or a source.",
      "- When sources disagree, give the range and say which source says what. When they do not cover part of the question, say so plainly and suggest what to check, instead of guessing.",
      "- Write like a knowledgeable local friend: concrete, opinionated where it helps, no filler. Use markdown: short ## headings, bullet points, **bold** key numbers, and a small table when comparing places or options.",
      "- Name real places, streets, listings and businesses from the sources when they help.",
      "- End with a short practical take: what you would do or check next.",
      "- Do not print a list of sources at the end; the app shows them as cards with photos.",
    ].join("\n"),
  };
}

/** The messages with the grounding system message first (the caller's own system messages after it). */
export function groundedMessages(messages: ChatMessage[], g: Grounding, today = new Date()): ChatMessage[] {
  return [groundingSystemMessage(g, today), ...messages];
}

/** The question a turn asks: the last user message. */
export function lastUserText(messages: ChatMessage[]): string {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i]!;
    if (m.role === "user" && typeof m.content === "string") return m.content;
  }
  return "";
}
