import { describe, expect, test } from "bun:test";
import { excerpt, groundedMessages, groundQuestion, needsWeb, parsePage, searchPlan, searchQuery, type Grounding } from "./web-grounding";
import { chatRunOpener, SOURCES_OBJECT } from "./run-opener";
import type { RunRequest } from "./runs";

const SF = "canyou tell me what the avareg cost for a halfway decent 2BR with off-street parkig, laundry and dishwasher on SF marina district? and then give me inner sunset too";

describe("web grounding", () => {
  test("factual questions are searched; code, writing and slash commands are not", () => {
    expect(needsWeb(SF)).toBe(true);
    expect(needsWeb("best ferry from sausalito to the ferry building")).toBe(true);
    expect(needsWeb("who won the election in 2026?")).toBe(true);
    expect(needsWeb("write a poem about the bay")).toBe(false);
    expect(needsWeb("/image a cat")).toBe(false);
    expect(needsWeb("```js\nconst x = 1\n```\nwhy does this fail?")).toBe(false);
    expect(needsWeb("translate hello to french")).toBe(false);
    expect(needsWeb("hi")).toBe(false);
  });

  test("a two-part question gets a search per place and a summary per place", () => {
    const p = searchPlan(SF);
    expect(p.subjects).toEqual(["sf marina", "inner sunset"]);
    expect(p.queries).toHaveLength(2);
    expect(p.queries[1]).toBe("inner sunset San Francisco avareg cost 2br parkig laundry dishwasher");
    expect(searchPlan("rent in noe valley vs the mission").subjects).toEqual(["noe valley", "mission"]);
    expect(searchPlan("who won the world series").queries).toHaveLength(1);
  });

  test("the search query drops chat filler", () => {
    expect(searchQuery("Hey, can you tell me the rent in the Marina?")).toBe("the rent in the Marina?");
    expect(searchQuery("x".repeat(500)).length).toBe(200);
  });

  test("a page gives its photo, title and the sentences that answer the question", () => {
    const html = `<html><head><title>Marina rents</title>
      <meta property="og:image" content="/img/marina.jpg"><meta name="description" content="Rents in the Marina &amp; Cow Hollow">
      <script>var junk = "$9,999 per month"</script></head>
      <body><nav>Home About</nav><p>The Marina is a neighborhood in San Francisco with views of the bay.</p>
      <p>The average rent for a 2-bedroom apartment in the Marina is $5,850 per month as of this fall.</p>
      <p>Cookies help us improve this site and remember your preferences across visits.</p></body></html>`;
    const p = parsePage(html, new URL("https://rents.example.com/marina"));
    expect(p.image).toBe("https://rents.example.com/img/marina.jpg");
    expect(p.title).toBe("Marina rents");
    expect(p.description).toBe("Rents in the Marina & Cow Hollow");
    expect(p.text).not.toContain("9,999");
    const ex = excerpt(p.text, "average rent 2BR marina");
    expect(ex).toContain("$5,850 per month");
    expect(ex).not.toContain("Cookies");
  });

  test("an http og:image or an svg is dropped (mixed content, tracking pixels)", () => {
    expect(parsePage(`<meta property="og:image" content="http://x.example.com/a.jpg">`, new URL("https://x.example.com/")).image).toBeNull();
    expect(parsePage(`<meta property="og:image" content="https://x.example.com/a.svg">`, new URL("https://x.example.com/")).image).toBeNull();
  });

  test("search down means no grounding, never an error", async () => {
    const fail = (async () => new Response("nope", { status: 503 })) as unknown as typeof fetch;
    expect(await groundQuestion("rent in the marina?", { fetch: fail })).toBeNull();
    const hang = ((_u: unknown, init?: RequestInit) =>
      new Promise((_r, rej) => init?.signal?.addEventListener("abort", () => rej(new DOMException("aborted", "AbortError"))))) as unknown as typeof fetch;
    const t0 = Date.now();
    expect(await groundQuestion("rent in the sunset?", { fetch: hang, budgetMs: 50 })).toBeNull();
    expect(Date.now() - t0).toBeLessThan(2000);
  });

  test("search hits become numbered sources, one per site, unreadable pages keep their snippet", async () => {
    const fake = (async (u: URL | string) => {
      const url = String(u);
      if (url.includes("/api/search"))
        return Response.json({
          results: [
            { url: "https://a.invalid/1", title: "A one", description: "2BR avg $5,500", host: "a.invalid" },
            { url: "https://a.invalid/2", title: "A two", description: "dup host", host: "a.invalid" },
            { url: "https://b.invalid/", title: "B", description: "Inner Sunset 2BR $4,200", host: "b.invalid" },
          ],
        });
      return new Response("blocked", { status: 403 });
    }) as unknown as typeof fetch;
    const g = await groundQuestion("2br rent marina vs inner sunset", { fetch: fake });
    expect(g?.sources.map((s) => [s.n, s.host])).toEqual([
      [1, "a.invalid"],
      [2, "b.invalid"],
    ]);
    expect(g?.context).toContain("[2] B (b.invalid)");
    expect(g?.context).toContain("Inner Sunset 2BR $4,200");
    const msgs = groundedMessages([{ role: "user", content: "q" }], g!);
    expect(msgs[0]!.role).toBe("system");
    expect(msgs[0]!.content).toContain("ONLY from the sources");
    expect(msgs[0]!.content).toContain("You are c0");
  });
});

describe("grounded run opener", () => {
  const sseAnswer = `data: {"choices":[{"delta":{"content":"About $5,500 [1]."}}]}\n\ndata: [DONE]\n\n`;
  const upstream = (async () => new Response(sseAnswer, { headers: { "content-type": "text/event-stream" } })) as never;
  const g: Grounding = { query: "q", sources: [{ n: 1, title: "A", url: "https://a.invalid/", host: "a.invalid", image: null, snippet: "s" }], context: "[1] A" };
  const req = (body: Record<string, unknown>): RunRequest => ({ userId: "u", coinpaySub: null, withPlugins: false, body: { messages: [{ role: "user", content: SF }], stream: true, ...body } });
  const read = async (s: ReadableStream<Uint8Array> | null) => (s ? await new Response(s).text() : "");

  test("web: true streams status, then sources, then the answer", async () => {
    let asked = "";
    const open = chatRunOpener(() => upstream, {
      ground: async (q, o) => {
        asked = q;
        o.onStatus("searching the web…");
        return g;
      },
    });
    const out = await read(await open(req({ web: true }), new AbortController().signal));
    expect(asked).toBe(SF);
    const iStatus = out.indexOf(": status");
    const iSources = out.indexOf(SOURCES_OBJECT);
    const iAnswer = out.indexOf("About $5,500");
    expect(iStatus).toBeGreaterThanOrEqual(0);
    expect(iSources).toBeGreaterThan(iStatus);
    expect(iAnswer).toBeGreaterThan(iSources);
  });

  test("API clients (no web flag) and failed grounding take the plain path", async () => {
    let grounded = 0;
    const open = chatRunOpener(() => upstream, {
      ground: async () => {
        grounded++;
        return null;
      },
    });
    expect(await read(await open(req({}), new AbortController().signal))).toBe(sseAnswer);
    expect(grounded).toBe(0);
    const out = await read(await open(req({ web: true }), new AbortController().signal));
    expect(grounded).toBe(1);
    expect(out).not.toContain(SOURCES_OBJECT);
    expect(out).toContain("About $5,500");
  });

  test("a network that refuses the turn ends the stream with a readable error", async () => {
    const down = (async () => Response.json({ error: { message: "Network is busy. Run a node or wait." } }, { status: 503 })) as never;
    const open = chatRunOpener(() => down, { ground: async () => g });
    const out = await read(await open(req({ web: true }), new AbortController().signal));
    expect(out).toContain("Network is busy");
    expect(out.trim().endsWith("[DONE]")).toBe(true);
  });
});
