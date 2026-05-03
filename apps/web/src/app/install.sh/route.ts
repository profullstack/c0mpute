/**
 * Serves the install script verbatim at /video/install.sh.
 *
 * In production this typically lives behind a CDN as a static asset, but
 * having it as a Next.js route lets us evolve the script without a separate
 * deploy. The canonical source-of-truth file is `scripts/install.sh`.
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";

const script = readFileSync(
  join(process.cwd(), "..", "..", "scripts", "install.sh"),
  "utf8",
);

export async function GET() {
  return new Response(script, {
    status: 200,
    headers: {
      "content-type": "text/plain; charset=utf-8",
      "cache-control": "public, max-age=300",
    },
  });
}
