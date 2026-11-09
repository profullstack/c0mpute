// @profullstack/c0mpute-captcha: solve captchas on the c0mpute network.
//
// Talks to POST /v1/captcha on a c0mpute buyer gateway (`c0mpute speech
// serve`), which auctions the job to a worker running ddddocr (image, slider,
// gap, detect) or whisper (audio), or answers math questions itself.
//
//   import { solveCaptcha } from "@profullstack/c0mpute-captcha";
//   const { text } = await fetch("https://example.com/captcha.png").then(solveCaptcha);
//
// Zero dependencies; Node 18+, Bun, Deno and browsers (anything with fetch).

export const DEFAULT_URL = "http://127.0.0.1:8787";
export const TOKEN_KINDS = ["recaptcha", "recaptcha_v2", "recaptcha_v3", "hcaptcha", "turnstile", "funcaptcha"];
export const KINDS = ["image", "slider", "gap", "detect", "audio", "math"];

/** A captcha this package cannot solve (token captchas: reCAPTCHA, hCaptcha, Turnstile...). */
export class UnsupportedCaptchaError extends Error {
  constructor(kind, detail = {}) {
    super(
      `${kind} is a token captcha: it is passed by a browser session, not read from a picture, ` +
        `and c0mpute does not solve it. Supported: ${KINDS.join(", ")}`,
    );
    this.name = "UnsupportedCaptchaError";
    this.kind = kind;
    Object.assign(this, detail);
  }
}

/** The gateway answered with an error. */
export class CaptchaError extends Error {
  constructor(message, status) {
    super(message);
    this.name = "CaptchaError";
    this.status = status;
  }
}

const env = (name) => {
  try {
    return (typeof process !== "undefined" && process.env && process.env[name]) || undefined;
  } catch {
    return undefined;
  }
};

// ─── bytes and base64 ──────────────────────────────────────────────────

function toBase64(bytes) {
  if (typeof Buffer !== "undefined") return Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength).toString("base64");
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

const isBytes = (v) => v instanceof Uint8Array || v instanceof ArrayBuffer || ArrayBuffer.isView(v);
const asU8 = (v) => (v instanceof Uint8Array ? v : v instanceof ArrayBuffer ? new Uint8Array(v) : new Uint8Array(v.buffer, v.byteOffset, v.byteLength));
const isBlob = (v) => typeof Blob !== "undefined" && v instanceof Blob;
const isResponse = (v) => typeof Response !== "undefined" && v instanceof Response;

/** Sniff what a byte buffer holds: "image", "audio" or null. */
export function sniff(bytes) {
  const b = asU8(bytes);
  const at = (i, ...xs) => xs.every((x, j) => b[i + j] === x);
  const ascii = (i, s) => [...s].every((c, j) => b[i + j] === c.charCodeAt(0));
  if (at(0, 0x89, 0x50, 0x4e, 0x47) || at(0, 0xff, 0xd8, 0xff) || ascii(0, "GIF8") || ascii(0, "BM")) return "image";
  if (ascii(0, "RIFF") && ascii(8, "WEBP")) return "image";
  if (ascii(0, "RIFF") && ascii(8, "WAVE")) return "audio";
  if (ascii(0, "ID3") || ascii(0, "OggS") || at(0, 0x1a, 0x45, 0xdf, 0xa3) || ascii(4, "ftyp")) return "audio";
  if (b[0] === 0xff && (b[1] & 0xe0) === 0xe0) return "audio";
  return null;
}

async function bytesOf(value, fetchImpl) {
  if (isBytes(value)) return asU8(value);
  if (isBlob(value)) return new Uint8Array(await value.arrayBuffer());
  if (isResponse(value)) return new Uint8Array(await value.arrayBuffer());
  if (typeof value === "string" && /^https?:\/\//i.test(value)) {
    const res = await fetchImpl(value);
    if (!res.ok) throw new CaptchaError(`could not fetch ${value}: HTTP ${res.status}`, res.status);
    return new Uint8Array(await res.arrayBuffer());
  }
  return null;
}

/** base64 (or data: URL) for an image/audio value: bytes, Blob, Response, URL, data URL or base64 string. */
async function encode(value, fetchImpl) {
  if (value == null) return undefined;
  if (typeof value === "string" && value.startsWith("data:")) return value.slice(value.indexOf(",") + 1);
  const bytes = await bytesOf(value, fetchImpl);
  if (bytes) return toBase64(bytes);
  if (typeof value === "string") return value.trim(); // already base64
  throw new TypeError("expected bytes, a Blob, a Response, a URL or a base64 string");
}

// ─── HTML pages: find the captcha ──────────────────────────────────────

const attr = (tag, name) => {
  const m = tag.match(new RegExp(`\\b${name}\\s*=\\s*("([^"]*)"|'([^']*)'|([^\\s>]+))`, "i"));
  return m ? (m[2] ?? m[3] ?? m[4]) : undefined;
};

/**
 * Look through an HTML page for a captcha. Returns
 *   { kind: "image", src }                        an <img> that looks like a captcha
 *   { kind: "recaptcha" | "hcaptcha" | "turnstile", siteKey }   a token captcha
 *   null                                          nothing found
 */
export function findCaptcha(html, baseUrl) {
  const token = [
    ["turnstile", /class\s*=\s*["'][^"']*cf-turnstile|challenges\.cloudflare\.com\/turnstile/i],
    ["hcaptcha", /class\s*=\s*["'][^"']*h-captcha|hcaptcha\.com\/1\/api\.js/i],
    ["recaptcha", /class\s*=\s*["'][^"']*g-recaptcha|google\.com\/recaptcha|recaptcha\.net\/recaptcha/i],
  ];
  for (const [kind, re] of token) {
    if (re.test(html)) {
      const key = html.match(/data-sitekey\s*=\s*["']([^"']+)["']/i)?.[1];
      return { kind, siteKey: key };
    }
  }
  for (const tag of html.match(/<img\b[^>]*>/gi) || []) {
    const src = attr(tag, "src");
    if (!src) continue;
    const hint = [src, attr(tag, "id"), attr(tag, "class"), attr(tag, "alt"), attr(tag, "name")].join(" ");
    if (/captcha|verif(y|ication)[-_ ]?code|security[-_ ]?code|kaptcha|securimage/i.test(hint)) {
      let resolved = src;
      try {
        resolved = new URL(src.replace(/&amp;/g, "&"), baseUrl).href;
      } catch {}
      return { kind: "image", src: resolved };
    }
  }
  return null;
}

/**
 * A short string that reads as an arithmetic question, not base64 or a URL.
 * Base64 never holds spaces or "?", and an image's base64 always holds letters.
 */
export function looksLikeQuestion(s) {
  if (s.length >= 256 || /^(https?:|data:)/i.test(s)) return false;
  const shaped = /[\s?]/.test(s) || /^[\d+\-*/x×÷=.]+$/.test(s);
  return shaped && /(\d|\b(plus|minus|times|divided|one|two|three|four|five|six|seven|eight|nine|ten)\b)/i.test(s);
}

// ─── the client ────────────────────────────────────────────────────────

/**
 * Make a solver bound to one gateway.
 * @param {object} [opts]
 * @param {string} [opts.url]    gateway base URL (env C0MPUTE_CAPTCHA_URL, default http://127.0.0.1:8787)
 * @param {string} [opts.token]  bearer token (env C0MPUTE_CAPTCHA_TOKEN or C0MPUTE_SPEECH_TOKEN)
 * @param {typeof fetch} [opts.fetch]
 * @param {number} [opts.timeoutMs]  default 90 s
 */
export function createSolver(opts = {}) {
  let base = opts.url || env("C0MPUTE_CAPTCHA_URL") || DEFAULT_URL;
  while (base.endsWith("/")) base = base.slice(0, -1);
  const token = opts.token || env("C0MPUTE_CAPTCHA_TOKEN") || env("C0MPUTE_SPEECH_TOKEN");
  const fetchImpl = opts.fetch || globalThis.fetch.bind(globalThis);
  const timeoutMs = opts.timeoutMs ?? 90_000;

  async function call(body) {
    const res = await fetchImpl(`${base}/v1/captcha`, {
      method: "POST",
      headers: { "content-type": "application/json", ...(token ? { authorization: `Bearer ${token}` } : {}) },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(timeoutMs),
    });
    const text = await res.text();
    let json;
    try {
      json = JSON.parse(text);
    } catch {
      json = null;
    }
    if (!res.ok) {
      const message = json?.error?.message || json?.error || json?.message || text || `HTTP ${res.status}`;
      throw new CaptchaError(`captcha gateway: ${typeof message === "string" ? message : JSON.stringify(message)}`, res.status);
    }
    return json;
  }

  const pick = (o) => {
    const out = {};
    for (const k of ["charset", "case", "length"]) if (o?.[k] != null) out[k] = o[k];
    return out;
  };

  /**
   * Solve a captcha. `input` may be:
   *  - a Response (image/*, audio/* or an HTML page holding an image captcha)
   *  - bytes, a Blob, an http(s) URL, a data: URL or a base64 string of an image or audio clip
   *  - a math question ("What is 3 + 4?")
   *  - an object {type, image, background, audio, question, charset, case, length}
   * Resolves to the gateway's answer: {kind, text | x,y | boxes, confidence?, ...}.
   */
  async function solve(input, options = {}) {
    if (input && typeof input === "object" && !isBytes(input) && !isBlob(input) && !isResponse(input)) {
      const { type, kind, image, background, audio, question, ...rest } = input;
      const k = (kind || type || (audio ? "audio" : question ? "math" : "image")).toLowerCase();
      if (TOKEN_KINDS.includes(k)) throw new UnsupportedCaptchaError(k);
      return call({
        type: k,
        image: await encode(image, fetchImpl),
        background: await encode(background, fetchImpl),
        audio: await encode(audio, fetchImpl),
        question,
        ...pick(rest),
        ...pick(options),
      });
    }
    if (isResponse(input)) {
      const ct = (input.headers.get("content-type") || "").toLowerCase();
      if (ct.includes("text/html")) {
        const html = await input.text();
        const found = findCaptcha(html, input.url || options.baseUrl);
        if (!found) throw new CaptchaError("no captcha found in the page", 0);
        if (found.kind !== "image") throw new UnsupportedCaptchaError(found.kind, { siteKey: found.siteKey });
        const img = await (options.fetchImage || fetchImpl)(found.src, options.imageInit);
        if (!img.ok) throw new CaptchaError(`could not fetch captcha image ${found.src}: HTTP ${img.status}`, img.status);
        const answer = await solve(new Uint8Array(await img.arrayBuffer()), options);
        return { ...answer, src: found.src };
      }
    }
    if (typeof input === "string" && looksLikeQuestion(input)) {
      return call({ type: "math", question: input, ...pick(options) });
    }
    if (typeof input === "string" && input.startsWith("data:audio")) {
      return call({ type: "audio", audio: await encode(input, fetchImpl), ...pick(options) });
    }
    const bytes = await bytesOf(input, fetchImpl);
    if (bytes) {
      const what = sniff(bytes) || "image";
      const b64 = toBase64(bytes);
      return call(what === "audio" ? { type: "audio", audio: b64, ...pick(options) } : { type: options.type || "image", image: b64, ...pick(options) });
    }
    return call({ type: options.type || "image", image: await encode(input, fetchImpl), ...pick(options) });
  }

  return {
    url: base,
    solve,
    /** Text in an image captcha. */
    image: (image, o = {}) => solve({ type: "image", image, ...o }),
    /** Where a puzzle piece fits its background: {x, y} (centre). */
    slider: (piece, background) => solve({ type: "slider", image: piece, background }),
    /** Where a backdrop with a hole differs from the clean one: {x, y}. */
    gap: (withGap, clean) => solve({ type: "gap", image: withGap, background: clean }),
    /** Character boxes in a click-the-text captcha: {boxes: [[x1,y1,x2,y2]]}. */
    detect: (image) => solve({ type: "detect", image }),
    /** Spoken digits/letters: {text, transcript}. */
    audio: (audio, o = {}) => solve({ type: "audio", audio, ...o }),
    /** Arithmetic questions, answered by the gateway itself. */
    math: (question) => solve({ type: "math", question }),
  };
}

let defaultSolver;
const solver = () => (defaultSolver ??= createSolver());

/**
 * Solve with the default gateway (env C0MPUTE_CAPTCHA_URL / C0MPUTE_CAPTCHA_TOKEN).
 * Works as a promise callback: `fetch(url).then(solveCaptcha)`.
 */
export function solveCaptcha(input, options) {
  // .then() passes only one argument; ignore anything that is not an options object.
  return solver().solve(input, options && typeof options === "object" ? options : {});
}

/** `fetch(url).then(solveWith({ charset: "digits" }))` */
export const solveWith = (options) => (input) => solveCaptcha(input, options);

export default solveCaptcha;
