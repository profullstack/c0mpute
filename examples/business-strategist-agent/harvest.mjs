#!/usr/bin/env node
// Build a training corpus from links.
//
//   node harvest.mjs https://rssamplifier.com/topics/business,saas,crypto --full --out data/corpus.jsonl
//   node harvest.mjs https://rssamplifier.com/topics/saas https://example.com/some-post --full
//   pbpaste | node harvest.mjs --links - --full          # a pasted list of links
//   node harvest.mjs --links links.txt --full            # one per line (or space/comma separated)
//   node harvest.mjs 'https://pit.world/search?q=saas,business,crypto' --full
//
// What each link becomes:
//   rssamplifier.com/topics/<a>[,<b>,...]  every feed filed under each topic -> their recent posts
//   rssamplifier.com/<feed-slug>           that one feed's recent posts
//   pit.world/search?q=<a>[,<b>,...]       with PIT_CORPUS_KEY: every page in pit.world's own index
//                                          matching any term, full text, paged from /api/corpus
//                                          (--pit-pages per link, publishers' AI-training opt-outs
//                                          already excluded). Without a key: each term searched on the
//                                          web, news and scholar tabs and every result page fetched.
//   anything else                          the page itself, as one document
//
// rssamplifier is public (no key): /api/topics, /api/feeds and the read_post tool
// on /api/mcp, which returns full article text (--full) and caches it, so a
// publisher is fetched once. Requests run 3 at a time.
// Output: JSONL, one {source, feed, guid, url, title, text} per document.

import { createWriteStream, readFileSync } from 'node:fs';

const argv = process.argv.slice(2);
const flags = new Set(['--full']);
const opts = {};
const links = [];
for (let i = 0; i < argv.length; i++) {
  const a = argv[i];
  if (flags.has(a)) opts[a.slice(2)] = true;
  else if (a.startsWith('--')) opts[a.slice(2)] = argv[++i];
  else links.push(a);
}
if (opts.links) {
  const raw = readFileSync(opts.links === '-' ? 0 : opts.links, 'utf8');
  links.push(...raw.split(/\s+|,(?=\s*https?:\/\/)/).filter((s) => /^https?:\/\//.test(s)));
}
if (!links.length) {
  console.error('usage: node harvest.mjs <url...> [--links file|-] [--full] [--feeds 200] [--posts 10] [--out data/corpus.jsonl]');
  process.exit(1);
}

const MAX_FEEDS = Number(opts.feeds ?? 200); // per topic
const POSTS_PER_FEED = Number(opts.posts ?? 10);
const GROUPS = String(opts.groups ?? 'blogs,news').split(',');
const FULL = Boolean(opts.full);
const OUT = opts.out ?? 'data/corpus.jsonl';
const MIN_CHARS = Number(opts['min-chars'] ?? 200);
const CONCURRENCY = 3;
const PIT_KEY = process.env.PIT_CORPUS_KEY;
const PIT_PAGES = Number(opts['pit-pages'] ?? 1000);
const corpusDocs = [];
const PIT_TABS = String(opts['pit-tabs'] ?? 'web,news,scholar').split(',');
const UA = 'business-strategist-agent harvester (+https://github.com/profullstack/c0mpute)';

async function fetchOk(url, init = {}) {
  for (let attempt = 1; ; attempt++) {
    const res = await fetch(url, {
      ...init,
      headers: { 'user-agent': UA, ...init.headers },
      signal: AbortSignal.timeout(60_000),
    }).catch((e) => e);
    if (res instanceof Response && res.ok) return res;
    if (attempt >= 3 || (res instanceof Response && res.status === 404)) {
      throw new Error(`${url}: ${res instanceof Response ? res.status : res.message}`);
    }
    await new Promise((r) => setTimeout(r, 2000 * attempt));
  }
}
const getJson = async (url, init) => (await fetchOk(url, init)).json();

const clean = (s) =>
  String(s ?? '')
    .replace(/<(script|style|nav|header|footer|aside|form|noscript)[\s\S]*?<\/\1>/gi, ' ')
    .replace(/<[^>]+>/g, ' ')
    .replace(/&nbsp;|&#160;/g, ' ')
    .replace(/&amp;/g, '&')
    .replace(/&quot;/g, '"')
    .replace(/&#39;|&rsquo;/g, "'")
    .replace(/\s+/g, ' ')
    .trim();

async function pool(items, fn) {
  let next = 0;
  await Promise.all(
    Array.from({ length: CONCURRENCY }, async () => {
      while (next < items.length) await fn(items[next++]);
    }),
  );
}

async function readPost(origin, feed, guid) {
  const rpc = await getJson(`${origin}/api/mcp`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'application/json, text/event-stream' },
    body: JSON.stringify({
      jsonrpc: '2.0',
      id: 1,
      method: 'tools/call',
      params: { name: 'read_post', arguments: { feed, guid } },
    }),
  });
  if (rpc.result?.isError) return null;
  const text = rpc.result?.content?.map((c) => c.text).join('\n') ?? '';
  try {
    return JSON.parse(text).text ?? null;
  } catch {
    return text;
  }
}

// --- work items: one per feed (rssamplifier) or per page (anything else) ---

const feedJobs = new Map(); // `${origin}|${slug}` -> {origin, slug, source}
const pageJobs = [];

for (const link of links) {
  const url = new URL(link);
  const parts = url.pathname.split('/').filter(Boolean);
  const isAmp = /(^|\.)rssamplifier\.com$/.test(url.hostname);
  if (isAmp && parts[0] === 'topics' && parts[1]) {
    for (const topic of parts[1].split(',').map((t) => t.trim()).filter(Boolean)) {
      let found = 0;
      for (const group of GROUPS) {
        for (let offset = 0; found < MAX_FEEDS; offset += 100) {
          const page = await getJson(`${url.origin}/api/topics/${topic}?group=${group}&limit=100&offset=${offset}`).catch(
            (e) => (console.error(`skip topic ${topic}/${group}: ${e.message}`), { feeds: [] }),
          );
          for (const f of page.feeds) {
            if (found >= MAX_FEEDS) break;
            const key = `${url.origin}|${f.slug}`;
            if (!feedJobs.has(key)) feedJobs.set(key, { origin: url.origin, slug: f.slug, source: `topic:${topic}` });
            found++;
          }
          if (page.feeds.length < 100) break;
        }
      }
      console.error(`topic ${topic}: ${found} feeds`);
    }
  } else if (/(^|\.)pit\.world$/.test(url.hostname) && parts[0] === 'search' && url.searchParams.get('q')) {
    // pit.world search returns 10 results per query and does not page, so a
    // comma list is one search per term, across several tabs.
    const terms = url.searchParams.get('q').split(',').map((t) => t.trim()).filter(Boolean);
    if (PIT_KEY) {
      // pit.world's own index, paged. Our key is free; others pay per page.
      let cursor = '';
      let got = 0;
      while (got < PIT_PAGES) {
        const qs = new URLSearchParams({ q: terms.join(','), limit: String(Math.min(100, PIT_PAGES - got)), ...(cursor ? { cursor } : {}) });
        const res = await getJson(`${url.origin}/api/corpus?${qs}`, { headers: { authorization: `Bearer ${PIT_KEY}` } }).catch(
          (e) => (console.error(`pit.world corpus: ${e.message}`), null),
        );
        if (!res) break;
        for (const it of res.items ?? []) {
          corpusDocs.push({ source: `pit-index:${terms.join(',')}`, feed: it.site, guid: null, url: it.url, title: clean(it.title), text: clean(it.text) });
          got++;
        }
        if (!res.next_cursor) break;
        cursor = res.next_cursor;
      }
      console.error(`pit.world index ${terms.join(',')}: ${got} pages`);
      continue;
    }
    const tabs = url.searchParams.get('t') ? [url.searchParams.get('t')] : PIT_TABS;
    for (const term of terms) {
      let found = 0;
      for (const tab of tabs) {
        const qs = new URLSearchParams({ q: term, ...(tab === 'web' ? {} : { t: tab }) });
        const res = await getJson(`${url.origin}/api/search?${qs}`).catch(
          (e) => (console.error(`skip search ${term}/${tab}: ${e.message}`), { results: [] }),
        );
        for (const r of res.results ?? []) {
          if (!r.url || pageJobs.some((j) => j.url === r.url)) continue;
          pageJobs.push({ url: r.url, source: `pit:${term}`, title: r.title, summary: r.description });
          found++;
        }
      }
      console.error(`pit.world ${term}: ${found} pages`);
    }
  } else if (isAmp && parts.length === 1) {
    feedJobs.set(`${url.origin}|${parts[0]}`, { origin: url.origin, slug: parts[0], source: `feed:${parts[0]}` });
  } else {
    pageJobs.push({ url: link, source: 'link' });
  }
}

const out = createWriteStream(OUT);
const seen = new Set();
let kept = 0;
let done = 0;
const total = feedJobs.size + pageJobs.length;

function write(doc) {
  const key = doc.url || doc.title.toLowerCase();
  if (doc.text.length < MIN_CHARS || seen.has(key) || seen.has(doc.title.toLowerCase())) return;
  seen.add(key);
  seen.add(doc.title.toLowerCase());
  out.write(JSON.stringify(doc) + '\n');
  kept++;
}
const tick = () => {
  if (++done % 25 === 0 || done === total) console.error(`${done}/${total} sources, ${kept} documents`);
};

for (const d of corpusDocs) write(d);

await pool([...feedJobs.values()], async ({ origin, slug, source }) => {
  try {
    const feed = await getJson(`${origin}/api/feeds/${slug}?limit=${POSTS_PER_FEED}`);
    // The directory is multilingual; keep English (or unlabelled) feeds so a
    // small model is not split across languages it cannot hold.
    if (feed.language && !/^en/i.test(feed.language)) return;
    for (const item of feed.items ?? []) {
      let text = clean(item.summary);
      if (FULL) text = clean(await readPost(origin, slug, item.guid).catch(() => null)) || text;
      write({ source, feed: slug, guid: item.guid, url: item.url, title: clean(item.title), text });
    }
  } catch (e) {
    console.error(`skip ${slug}: ${e.message}`);
  } finally {
    tick();
  }
});

await pool(pageJobs, async ({ url: link, source, title: knownTitle, summary }) => {
  try {
    const res = await fetchOk(link);
    if (!/html|text\/plain/i.test(res.headers.get('content-type') ?? '')) throw new Error('not a web page');
    const html = await res.text();
    const title = clean(knownTitle ?? html.match(/<title[^>]*>([\s\S]*?)<\/title>/i)?.[1] ?? link);
    const body = html.match(/<(article|main)[\s\S]*?<\/\1>/i)?.[0] ?? html.match(/<body[\s\S]*<\/body>/i)?.[0] ?? html;
    write({ source, feed: null, guid: null, url: link, title, text: clean(body).slice(0, 24_000) });
  } catch (e) {
    // A blocked or non-HTML page still has its search snippet.
    if (summary) write({ source, feed: null, guid: null, url: link, title: clean(knownTitle), text: clean(summary) });
    else console.error(`skip ${link}: ${e.message}`);
  } finally {
    tick();
  }
});

out.end();
console.error(`wrote ${kept} documents to ${OUT}`);
