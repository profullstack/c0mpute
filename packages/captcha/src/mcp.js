// stdio MCP server (JSON-RPC 2.0, one message per line) exposing solve_captcha.
// Zero dependencies. Run: `c0mpute-captcha mcp` (or `npx @profullstack/c0mpute-captcha mcp`).
import { readFile } from "node:fs/promises";
import { createInterface } from "node:readline";
import { createSolver, UnsupportedCaptchaError } from "./index.js";

export const PROTOCOL_VERSION = "2025-06-18";

const source = {
  type: "string",
  description: "An http(s) URL, a local file path, a data: URL or base64.",
};

export const TOOLS = [
  {
    name: "solve_captcha",
    description:
      "Solve a captcha on the c0mpute network. type=image reads the text in a picture; slider/gap find where a puzzle piece fits (centre x,y); detect returns character boxes; audio transcribes spoken digits/letters; math answers an arithmetic question. Token captchas (reCAPTCHA, hCaptcha, Turnstile) are not supported.",
    inputSchema: {
      type: "object",
      properties: {
        type: { type: "string", enum: ["image", "slider", "gap", "detect", "audio", "math"], description: "Default: image (or audio/math when only those fields are given)." },
        image: { ...source, description: "The captcha image; the puzzle piece for slider; the backdrop with the gap for gap. " + source.description },
        background: { ...source, description: "slider: the background; gap: the clean backdrop. " + source.description },
        audio: { ...source, description: "Audio captcha clip (WAV, MP3, Ogg, WebM, M4A). " + source.description },
        question: { type: "string", description: "math: the question, e.g. 'What is 3 + 4?'" },
        page_url: { type: "string", description: "An HTML page to find an image captcha in (fetched without cookies)." },
        charset: { type: "string", description: "image: digits, lower, upper, letters, alnum, lower_digits, upper_digits, or literal characters." },
        case: { type: "string", enum: ["lower", "upper", "any"] },
        length: { type: "number", description: "Expected answer length; the result says length_ok." },
      },
    },
  },
];

/** A file path is read from disk; URLs, data: URLs and base64 pass through. */
export async function load(v) {
  if (typeof v !== "string" || v.length > 4096 || /^(https?:|data:)/i.test(v)) return v;
  const path = v.startsWith("~/") ? `${process.env.HOME}${v.slice(1)}` : v;
  try {
    return new Uint8Array(await readFile(path));
  } catch (e) {
    if (e && (e.code === "ENOENT" || e.code === "ENAMETOOLONG" || e.code === "EISDIR")) return v;
    throw e;
  }
}

/** One JSON-RPC message in, one response out (null for notifications). */
export async function handleRpc(msg, solver) {
  const { id, method, params = {} } = msg || {};
  const ok = (result) => ({ jsonrpc: "2.0", id: id ?? null, result });
  const fail = (code, message) => ({ jsonrpc: "2.0", id: id ?? null, error: { code, message } });
  const text = (t, isError = false) => ok({ content: [{ type: "text", text: t }], ...(isError ? { isError } : {}) });
  switch (method) {
    case "initialize":
      return ok({
        protocolVersion: PROTOCOL_VERSION,
        capabilities: { tools: { listChanged: false } },
        serverInfo: { name: "c0mpute-captcha", version: "0.1.0" },
        instructions: `Solve captchas on c0mpute via ${solver.url}/v1/captcha. Call solve_captcha.`,
      });
    case "ping":
      return ok({});
    case "tools/list":
      return ok({ tools: TOOLS });
    case "tools/call": {
      if (params.name !== "solve_captcha") return fail(-32602, `unknown tool: ${params.name}`);
      const a = params.arguments || {};
      try {
        let result;
        if (a.page_url) {
          const res = await fetch(a.page_url);
          result = await solver.solve(res, { charset: a.charset, case: a.case, length: a.length });
        } else {
          result = await solver.solve({
            type: a.type,
            image: await load(a.image),
            background: await load(a.background),
            audio: await load(a.audio),
            question: a.question,
            charset: a.charset,
            case: a.case,
            length: a.length,
          });
        }
        return text(JSON.stringify(result));
      } catch (e) {
        const extra = e instanceof UnsupportedCaptchaError && e.siteKey ? ` (siteKey ${e.siteKey})` : "";
        return text(`${e instanceof Error ? e.message : String(e)}${extra}`, true);
      }
    }
    default:
      if (id === undefined) return null;
      return fail(-32601, `method not found: ${method}`);
  }
}

export async function serveStdio(opts = {}) {
  const solver = createSolver(opts);
  const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
  for await (const line of rl) {
    if (!line.trim()) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error" } }) + "\n");
      continue;
    }
    const reply = await handleRpc(msg, solver);
    if (reply) process.stdout.write(JSON.stringify(reply) + "\n");
  }
}
