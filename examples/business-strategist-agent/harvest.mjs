#!/usr/bin/env node
// Build a training corpus from an rssamplifier topic page.
//
//   node harvest.mjs https://rssamplifier.com/topics/business --feeds 200 --posts 10 --full
//
// Every feed filed under the topic becomes training data:
//   1. /api/topics/<slug>?group=<group>  pages through the feeds on the topic
//   2. /api/feeds/<feed>                 lists each feed's recent posts
//   3. read_post on /api/mcp             (with --full) fetches the article text;
//                                        without it we train on title + summary
//
// No key needed: the directory is public. Requests run 3 at a time so we are
// polite to rssamplifier and, through read_post, to the publishers behind it.
// Output: one JSON object per line, {feed, guid, url, title, text}.

import { createWriteStream } from 'node:fs';

const args = process.argv.slice(2);
const topicUrl = args.find((a) => a.startsWith('http')) ?? 'https://rssamplifier.com/topics/business';
const opt = (name, def) => {
  const i = args.indexOf(`--${name}`);
  return i === -1 ? def : args[i + 1];
};
const MAX_FEEDS = Number(opt('feeds', 200));
const POSTS_PER_FEED = Number(opt('posts', 10));
const GROUPS = String(opt('groups', 'blogs,news')).split(',');
const FULL = args.includes('--full');
const OUT = opt('out', 'corpus.jsonl');
const MIN_CHARS = Number(opt('min-chars', 200));
const CONCURRENCY = 3;

const { origin, pathname } = new URL(topicUrl);
const topic = pathname.split('/').filter(Boolean).pop();

async function getJson(url, init) {
  for (let attempt = 1; ; attempt++) {
    const res = await fetch(url, { ...init, signal: AbortSignal.timeout(60_000) }).catch((e) => e);
    if (res instanceof Response && res.ok) return res.json();
    if (attempt >= 3) throw new Error(`${url}: ${res instanceof Response ? res.status : res.message}`);
    await new Promise((r) => setTimeout(r, 2000 * attempt));
  }
}

async function topicFeeds() {
  const feeds = new Map();
  for (const group of GROUPS) {
    for (let offset = 0; feeds.size < MAX_FEEDS; offset += 100) {
      const page = await getJson(`${origin}/api/topics/${topic}?group=${group}&limit=100&offset=${offset}`);
      for (const f of page.feeds) if (feeds.size < MAX_FEEDS) feeds.set(f.slug, f);
      if (page.feeds.length < 100) break;
    }
  }
  return [...feeds.values()];
}

async function readPost(feed, guid) {
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
    const parsed = JSON.parse(text);
    return parsed.text ?? parsed.content ?? parsed.body ?? null;
  } catch {
    return text;
  }
}

const clean = (s) =>
  String(s ?? '')
    .replace(/<[^>]+>/g, ' ')
    .replace(/&nbsp;|&#160;/g, ' ')
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

const feeds = await topicFeeds();
console.error(`${topic}: ${feeds.length} feeds (${GROUPS.join(', ')})`);

const out = createWriteStream(OUT);
const seen = new Set();
let kept = 0;
let done = 0;

await pool(feeds, async (f) => {
  try {
    const feed = await getJson(`${origin}/api/feeds/${f.slug}?limit=${POSTS_PER_FEED}`);
    // The directory is multilingual; keep English (or unlabelled) feeds so a
    // 135M model is not split across languages it cannot hold.
    if (feed.language && !/^en/i.test(feed.language)) return;
    for (const item of feed.items ?? []) {
      const title = clean(item.title);
      let text = clean(item.summary);
      if (FULL) text = clean(await readPost(f.slug, item.guid).catch(() => null)) || text;
      const key = title.toLowerCase();
      if (text.length < MIN_CHARS || seen.has(key)) continue;
      seen.add(key);
      out.write(JSON.stringify({ feed: f.slug, guid: item.guid, url: item.url, title, text }) + '\n');
      kept++;
    }
  } catch (e) {
    console.error(`skip ${f.slug}: ${e.message}`);
  } finally {
    if (++done % 25 === 0) console.error(`${done}/${feeds.length} feeds, ${kept} documents`);
  }
});

out.end();
console.error(`wrote ${kept} documents to ${OUT}`);
