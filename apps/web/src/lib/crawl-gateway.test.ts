import { describe, expect, test } from "bun:test";
import { gate } from "./crawl-gateway";

// The crawl gateway in front of c0mpute.com (proxy.ts): AI training crawlers pay, sitemaps stay open.
const GPTBOT = "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; GPTBot/1.2; +https://openai.com/gptbot)";
const BROWSER = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";
const req = (path: string, headers: Record<string, string>) => new Request(`https://c0mpute.com${path}`, { headers });

describe("crawl gateway (x402-gateway >= 0.18.4)", () => {
  test("a declared training crawler pays for a page", async () => {
    const res = await gate(req("/", { "user-agent": GPTBOT }));
    expect(res?.status).toBe(402);
  });

  test("a forged session cookie does not let a declared crawler read free (0.18.3)", async () => {
    const res = await gate(req("/estimates", { "user-agent": GPTBOT, cookie: "c0_session=forged; session=forged" }));
    expect(res?.status).toBe(402);
  });

  test("sitemaps are readable by every crawler, like robots.txt (0.18.4)", async () => {
    for (const path of ["/sitemap.xml", "/sitemap_index.xml", "/sitemap-0.xml", "/robots.txt"]) {
      const res = await gate(req(path, { "user-agent": GPTBOT }));
      expect(res === undefined || res.status !== 402).toBe(true);
    }
  });

  test("a person passes through", async () => {
    const res = await gate(req("/", { "user-agent": BROWSER, accept: "text/html" }));
    expect(res === undefined || res.status !== 402).toBe(true);
  });
});
