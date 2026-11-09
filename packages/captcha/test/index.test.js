import { test } from "node:test";
import assert from "node:assert/strict";
import { createSolver, findCaptcha, looksLikeQuestion, sniff, UnsupportedCaptchaError, CaptchaError } from "../src/index.js";
import { handleRpc } from "../src/mcp.js";

const PNG = Uint8Array.from(Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=", "base64"));
const WAV = Uint8Array.from([...Buffer.from("RIFF"), 0, 0, 0, 0, ...Buffer.from("WAVEfmt ")]);

/** A fake gateway that records requests and answers like /v1/captcha. */
function gateway(overrides = {}) {
  const calls = [];
  const fetch = async (url, init = {}) => {
    if (overrides[url]) return overrides[url]();
    calls.push({ url, init, body: init.body ? JSON.parse(init.body) : undefined });
    const body = calls.at(-1).body;
    if (body.type === "recaptcha") return new Response(JSON.stringify({ error: { message: "token captcha" } }), { status: 501 });
    const answer = { v: 1, ok: true, kind: body.type };
    if (body.type === "slider" || body.type === "gap") Object.assign(answer, { x: 115, y: 75 });
    else if (body.type === "detect") answer.boxes = [[1, 2, 3, 4]];
    else answer.text = body.type === "math" ? "7" : "ab12";
    return new Response(JSON.stringify(answer), { headers: { "content-type": "application/json" } });
  };
  return { calls, solver: createSolver({ url: "http://gw:8787/", token: "t".repeat(32), fetch }) };
}

test("bytes: image goes to /v1/captcha as base64 with the bearer token", async () => {
  const { calls, solver } = gateway();
  const r = await solver.solve(PNG, { charset: "digits", case: "upper" });
  assert.equal(r.text, "ab12");
  assert.equal(calls[0].url, "http://gw:8787/v1/captcha");
  assert.equal(calls[0].init.headers.authorization, `Bearer ${"t".repeat(32)}`);
  assert.deepEqual(calls[0].body, { type: "image", image: Buffer.from(PNG).toString("base64"), charset: "digits", case: "upper" });
});

test("fetch(url).then(solve): an image Response, an audio Response", async () => {
  const { calls, solver } = gateway();
  await solver.solve(new Response(PNG, { headers: { "content-type": "image/png" } }));
  assert.equal(calls[0].body.type, "image");
  await solver.solve(new Response(WAV, { headers: { "content-type": "audio/wav" } }));
  assert.equal(calls[1].body.type, "audio");
  assert.ok(calls[1].body.audio);
});

test("HTML Response: finds the captcha <img>, fetches it relative to the page", async () => {
  const { calls, solver } = gateway({ "https://site.test/captcha.php?id=9&x=1": () => new Response(PNG) });
  const page = new Response('<form><img id="captchaImg" src="/captcha.php?id=9&amp;x=1"></form>', { headers: { "content-type": "text/html" } });
  Object.defineProperty(page, "url", { value: "https://site.test/login" });
  const r = await solver.solve(page);
  assert.equal(r.src, "https://site.test/captcha.php?id=9&x=1");
  assert.equal(calls[0].body.type, "image");
});

test("HTML Response with reCAPTCHA / Turnstile: UnsupportedCaptchaError with the site key", async () => {
  const { solver } = gateway();
  const page = new Response('<div class="g-recaptcha" data-sitekey="6Lc_abc"></div>', { headers: { "content-type": "text/html" } });
  await assert.rejects(solver.solve(page), (e) => e instanceof UnsupportedCaptchaError && e.kind === "recaptcha" && e.siteKey === "6Lc_abc");
  await assert.rejects(solver.solve({ type: "hcaptcha" }), UnsupportedCaptchaError);
});

test("questions, slider, gap, detect, gateway errors", async () => {
  const { calls, solver } = gateway();
  assert.equal((await solver.solve("What is 3 + 4?")).text, "7");
  assert.equal(calls[0].body.type, "math");
  assert.deepEqual(await solver.slider(PNG, PNG).then((r) => [r.x, r.y]), [115, 75]);
  assert.equal(calls[1].body.background, Buffer.from(PNG).toString("base64"));
  assert.equal((await solver.gap(PNG, PNG)).x, 115);
  assert.deepEqual((await solver.detect(PNG)).boxes, [[1, 2, 3, 4]]);
  await solver.solve("data:image/png;base64,QUJD");
  assert.equal(calls.at(-1).body.image, "QUJD");
  const bad = createSolver({ url: "http://gw", fetch: async () => new Response('{"error":{"message":"no worker"}}', { status: 503 }) });
  await assert.rejects(bad.image(PNG), (e) => e instanceof CaptchaError && e.status === 503 && /no worker/.test(e.message));
});

test("helpers", () => {
  assert.equal(sniff(PNG), "image");
  assert.equal(sniff(WAV), "audio");
  assert.equal(sniff(new Uint8Array([1, 2, 3, 4])), null);
  assert.ok(looksLikeQuestion("What is 3 + 4?"));
  assert.ok(looksLikeQuestion("3+4="));
  assert.ok(!looksLikeQuestion(Buffer.from(PNG).toString("base64")));
  assert.ok(!looksLikeQuestion("https://x.test/a?b=1"));
  assert.deepEqual(findCaptcha('<img src="logo.png"><img alt="security code" src="c.jpg">', "https://a.test/x/"), { kind: "image", src: "https://a.test/x/c.jpg" });
  assert.equal(findCaptcha('<script src="https://challenges.cloudflare.com/turnstile/v0/api.js"></script>').kind, "turnstile");
  assert.equal(findCaptcha("<p>hi</p>"), null);
});

test("MCP: initialize, tools/list, tools/call", async () => {
  const { calls, solver } = gateway();
  const init = await handleRpc({ jsonrpc: "2.0", id: 1, method: "initialize" }, solver);
  assert.equal(init.result.serverInfo.name, "c0mpute-captcha");
  const list = await handleRpc({ id: 2, method: "tools/list" }, solver);
  assert.equal(list.result.tools[0].name, "solve_captcha");
  const call = await handleRpc({ id: 3, method: "tools/call", params: { name: "solve_captcha", arguments: { image: Buffer.from(PNG).toString("base64"), charset: "alnum" } } }, solver);
  assert.equal(JSON.parse(call.result.content[0].text).text, "ab12");
  assert.equal(calls[0].body.charset, "alnum");
  const bad = await handleRpc({ id: 4, method: "tools/call", params: { name: "solve_captcha", arguments: { type: "turnstile" } } }, solver);
  assert.equal(bad.result.isError, true);
  assert.equal(await handleRpc({ method: "notifications/initialized" }, solver), null);
});
