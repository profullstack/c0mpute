#!/usr/bin/env node
// c0mpute-captcha: solve a captcha on the c0mpute network from the shell, or
// run the stdio MCP server.
import { readFile } from "node:fs/promises";
import { createSolver } from "../src/index.js";
import { serveStdio } from "../src/mcp.js";

const HELP = `c0mpute-captcha: solve captchas on the c0mpute network

Usage:
  c0mpute-captcha <file|url|->           text in an image captcha (or audio clip)
  c0mpute-captcha --type slider piece.png --background bg.png
  c0mpute-captcha --type gap with-gap.png --background clean.png
  c0mpute-captcha --type detect click.png
  c0mpute-captcha --question "What is 3 + 4?"
  c0mpute-captcha --page https://example.com/login   find + solve an image captcha
  c0mpute-captcha mcp                    stdio MCP server (tool: solve_captcha)

Options:
  --type <kind>        image (default), slider, gap, detect, audio, math
  --background <src>   slider/gap background
  --charset <set>      digits, lower, upper, letters, alnum, lower_digits, upper_digits, or literal chars
  --case lower|upper   force the answer's case
  --length <n>         expected length (adds length_ok)
  --json               print the full JSON answer
  --url <gateway>      default $C0MPUTE_CAPTCHA_URL or http://127.0.0.1:8787
  --token <token>      default $C0MPUTE_CAPTCHA_TOKEN or $C0MPUTE_SPEECH_TOKEN

The gateway is \`c0mpute speech serve\` (C0MPUTE_SPEECH_TOKEN=... c0mpute speech serve).
`;

function parse(argv) {
  const o = { _: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "-h" || a === "--help") o.help = true;
    else if (a === "--json") o.json = true;
    else if (a.startsWith("--")) {
      const [k, v] = a.includes("=") ? a.slice(2).split(/=(.*)/s) : [a.slice(2), argv[++i]];
      if (v === undefined) throw new Error(`--${k} needs a value`);
      o[k] = v;
    } else o._.push(a);
  }
  return o;
}

async function source(v) {
  if (v === undefined) return undefined;
  if (v === "-") {
    const chunks = [];
    for await (const c of process.stdin) chunks.push(c);
    return new Uint8Array(Buffer.concat(chunks));
  }
  if (/^(https?:|data:)/i.test(v)) return v;
  return new Uint8Array(await readFile(v));
}

async function main() {
  const o = parse(process.argv.slice(2));
  if (o.help || (!o._.length && !o.question && !o.page)) {
    process.stdout.write(HELP);
    return;
  }
  const solver = createSolver({ url: o.url, token: o.token });
  if (o._[0] === "mcp") return serveStdio({ url: o.url, token: o.token });
  const opts = { charset: o.charset, case: o.case, length: o.length ? Number(o.length) : undefined };
  let result;
  if (o.page) {
    result = await solver.solve(await fetch(o.page), opts);
  } else if (o.question) {
    result = await solver.math(o.question);
  } else {
    const input = await source(o._[0]);
    result = o.type || o.background
      ? await solver.solve({ type: o.type || "image", image: input, background: await source(o.background), ...opts })
      : await solver.solve(input, opts);
  }
  if (o.json) console.log(JSON.stringify(result, null, 2));
  else if (result.boxes) console.log(JSON.stringify(result.boxes));
  else if (result.text !== undefined) console.log(result.text);
  else console.log(`${result.x},${result.y}`);
}

main().catch((e) => {
  console.error(`c0mpute-captcha: ${e instanceof Error ? e.message : e}`);
  process.exit(1);
});
