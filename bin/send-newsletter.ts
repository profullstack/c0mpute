#!/usr/bin/env bun
/**
 * Send a newsletter to all subscribers listed in a file.
 *
 * Usage:
 *   bun bin/send-newsletter.ts --subject "Hello" --body "Body text here"
 *   bun bin/send-newsletter.ts --subject "Hello" --body "Body text" --list ./subscribers.txt
 *   bun bin/send-newsletter.ts --subject "Hello" --body "Body text" --dry-run
 *
 * Subscribers file: one email per line, blank lines and # comments ignored.
 * Requires RESEND_API_KEY in .env or environment.
 */

import { parseArgs } from "node:util";
import { readFileSync, existsSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

// Bun auto-loads .env, but load manually if running outside of bun
if (!process.env.RESEND_API_KEY) {
  const envPath = resolve(ROOT, ".env");
  if (existsSync(envPath)) {
    for (const line of readFileSync(envPath, "utf8").split("\n")) {
      const trimmed = line.trim();
      if (!trimmed || trimmed.startsWith("#")) continue;
      const eq = trimmed.indexOf("=");
      if (eq === -1) continue;
      const key = trimmed.slice(0, eq).trim();
      const val = trimmed.slice(eq + 1).trim();
      if (key && !(key in process.env)) process.env[key] = val;
    }
  }
}

const { values } = parseArgs({
  args: process.argv.slice(2),
  options: {
    subject: { type: "string" },
    body: { type: "string" },
    list: { type: "string", default: resolve(ROOT, "subscribers.txt") },
    from: { type: "string" },
    "dry-run": { type: "boolean", default: false },
    help: { type: "boolean", default: false },
  },
  allowPositionals: false,
});

if (values.help || !values.subject || !values.body) {
  console.log(`
Usage: bun bin/send-newsletter.ts --subject <subject> --body <body> [options]

Required:
  --subject <text>   Email subject line
  --body <text>      Plain-text body (each \\n becomes a <p> in HTML)

Optional:
  --list <file>      Subscriber list file (default: ./subscribers.txt)
  --from <address>   Override sender (default: EMAIL_FROM env var)
  --dry-run          Print recipients and exit without sending
  --help             Show this help
`);
  process.exit(values.help ? 0 : 1);
}

const RESEND_API_KEY = process.env.RESEND_API_KEY;
if (!RESEND_API_KEY) {
  console.error("Error: RESEND_API_KEY is not set. Add it to .env or the environment.");
  process.exit(1);
}

const listFile = values.list as string;
if (!existsSync(listFile)) {
  console.error(`Error: subscriber list not found: ${listFile}`);
  console.error("Create it with one email per line (# comments and blank lines are ignored).");
  process.exit(1);
}

const emails = readFileSync(listFile, "utf8")
  .split("\n")
  .map((l) => l.trim())
  .filter((l) => l && !l.startsWith("#") && l.includes("@"));

if (emails.length === 0) {
  console.error(`Error: no valid emails found in ${listFile}`);
  process.exit(1);
}

const fromName = process.env.EMAIL_FROM_NAME ?? "c0mpute";
const fromAddr = values.from ?? process.env.EMAIL_FROM ?? "hello@c0mpute.com";
const from = `${fromName} <${fromAddr}>`;
const subject = values.subject as string;
const body = values.body as string;

const html = body
  .split("\n")
  .filter((l) => l.trim())
  .map((l) => `<p style="margin:0 0 1em">${l}</p>`)
  .join("\n");

if (values["dry-run"]) {
  console.log(`Dry run — would send to ${emails.length} recipient(s):`);
  for (const e of emails) console.log(`  ${e}`);
  console.log(`\nFrom:    ${from}`);
  console.log(`Subject: ${subject}`);
  console.log(`\n${body}`);
  process.exit(0);
}

console.log(`Sending "${subject}" to ${emails.length} recipient(s)…`);

let sent = 0;
let failed = 0;

for (const to of emails) {
  try {
    const res = await fetch("https://api.resend.com/emails", {
      method: "POST",
      headers: {
        Authorization: `Bearer ${RESEND_API_KEY}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ from, to, subject, html, text: body }),
    });

    if (!res.ok) {
      const err = await res.text();
      console.error(`  ✗ ${to} — ${res.status} ${err}`);
      failed++;
    } else {
      console.log(`  ✓ ${to}`);
      sent++;
    }
  } catch (err) {
    console.error(`  ✗ ${to} — ${err instanceof Error ? err.message : err}`);
    failed++;
  }
}

console.log(`\nDone. sent=${sent} failed=${failed}`);
if (failed > 0) process.exit(1);
