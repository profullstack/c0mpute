# @profullstack/c0mpute-captcha

Solve captchas on the [c0mpute](https://c0mpute.com) network. Workers read
them on plain CPUs with [ddddocr](https://github.com/sml2h3/ddddocr) (image
text, slider and gap puzzles, click-the-text boxes) or whisper (audio
captchas). The gateway answers math questions itself.

You get a library, a CLI and an MCP server. It has zero dependencies and runs
on Node 18+, Bun, Deno and browsers.

## Run a gateway

The client talks to `POST /v1/captcha` on a c0mpute buyer gateway:

```sh
curl -fsSL https://c0mpute.com/install.sh | sh
export C0MPUTE_SPEECH_TOKEN=$(openssl rand -hex 24)
c0mpute speech serve            # listens on 127.0.0.1:8787
```

Point the client at it with `C0MPUTE_CAPTCHA_URL` (default
`http://127.0.0.1:8787`) and `C0MPUTE_CAPTCHA_TOKEN` (falls back to
`C0MPUTE_SPEECH_TOKEN`).

## Library

```js
import { solveCaptcha, solveWith, createSolver } from "@profullstack/c0mpute-captcha";

// An image (or audio) URL straight into the solver:
const { text } = await fetch("https://example.com/captcha.png").then(solveCaptcha);

// Limit the charset, force case, check length:
await fetch(url).then(solveWith({ charset: "digits", length: 4 }));

// An HTML page: finds the captcha <img> and fetches it. Pass a cookie-carrying
// fetch so the image belongs to your session.
const answer = await fetch("https://example.com/login").then((res) =>
  solveCaptcha(res, { fetchImage: myCookieFetch }),
);

const solver = createSolver({ url: "http://127.0.0.1:8787", token: process.env.C0MPUTE_SPEECH_TOKEN });
await solver.image(bytes, { charset: "alnum" });     // { text, confidence }
await solver.slider(pieceBytes, backgroundBytes);    // { x, y } centre of the fit
await solver.gap(withGapBytes, cleanBytes);          // { x, y }
await solver.detect(clickCaptchaBytes);              // { boxes: [[x1,y1,x2,y2]] }
await solver.audio(wavBytes);                        // { text: "7421", transcript }
await solver.math("What is 3 + 4?");                 // { text: "7" }
```

Inputs can be bytes, a `Blob`, a `Response`, an http(s) URL, a `data:` URL or
base64.

Token captchas (reCAPTCHA, hCaptcha, Turnstile, FunCaptcha) throw
`UnsupportedCaptchaError` with the page's `siteKey`. A browser session passes
those; you can't read them from a picture, and c0mpute does not solve them.

## CLI

```sh
npx @profullstack/c0mpute-captcha code.png --charset alnum
npx @profullstack/c0mpute-captcha https://example.com/captcha.jpg
npx @profullstack/c0mpute-captcha --type slider piece.png --background bg.png
npx @profullstack/c0mpute-captcha --question "What is 3 + 4?"
npx @profullstack/c0mpute-captcha --page https://example.com/login --json
```

The node binary has the same thing built in: `c0mpute captcha code.png --charset alnum --plain`.

## MCP

```sh
claude mcp add c0mpute-captcha -- npx -y @profullstack/c0mpute-captcha mcp
```

It provides one tool, `solve_captcha`, which takes `type`, `image`,
`background`, `audio`, `question`, `page_url`, `charset`, `case` and `length`.
Images can be file paths, URLs or base64.

## HTTP

```sh
curl -s localhost:8787/v1/captcha -H "authorization: Bearer $C0MPUTE_SPEECH_TOKEN" \
  -H 'content-type: application/json' \
  -d "{\"type\":\"image\",\"image\":\"$(base64 -w0 code.png)\",\"charset\":\"digits\"}"
```

## Accuracy

ddddocr's general model reads clean or lightly distorted text captchas well.
Heavily warped, overlapping or site-specific styles miss more often. Use
`charset`, `length` and `confidence` to decide when to fetch a fresh captcha
and retry.

MIT
