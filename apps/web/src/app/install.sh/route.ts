/**
 * Serves the canonical c0mpute installer at https://c0mpute.com/install.sh.
 *
 * The script body is the file at `scripts/install.sh` in the repo —
 * read at request time so we don't need a separate deploy when we tweak
 * it. Same-shape mechanism used for `/plugins/<id>/install.sh`.
 */

import { readFileSync } from "node:fs";
import path from "node:path";

const INSTALL_SH_PATH = path.resolve(
  process.cwd(),
  "..",
  "..",
  "scripts",
  "install.sh",
);

export async function GET() {
  let script: string;
  try {
    script = readFileSync(INSTALL_SH_PATH, "utf8");
  } catch {
    return new Response("install.sh not found", { status: 500 });
  }
  return new Response(script, {
    status: 200,
    headers: {
      "content-type": "text/plain; charset=utf-8",
      "cache-control": "public, max-age=300, s-maxage=300",
    },
  });
}
