/**
 * Serves a plugin's install.sh verbatim from `plugins/<id>/install.sh`.
 *
 * Hit at https://c0mpute.com/plugins/<id>/install.sh — chained to by
 * `c0mpute plugin install <id>` and by users running:
 *   curl -fsSL https://c0mpute.com/plugins/<id>/install.sh | sh
 *
 * The same script is also reachable directly from GitHub raw at
 *   https://raw.githubusercontent.com/profullstack/c0mpute/master/plugins/<id>/install.sh
 * — useful if c0mpute.com is down. Both are served from the exact same
 * file in the repo.
 */

import { readFileSync, existsSync } from "node:fs";
import path from "node:path";

const PLUGINS_DIR = path.resolve(process.cwd(), "..", "..", "plugins");

export async function GET(
  _req: Request,
  { params }: { params: Promise<{ id: string }> },
) {
  const { id } = await params;

  // Reject anything that could escape the plugins directory.
  if (!/^[a-z][a-z0-9_-]*$/i.test(id)) {
    return new Response("invalid plugin id", { status: 400 });
  }

  const file = path.join(PLUGINS_DIR, id, "install.sh");
  if (!existsSync(file)) {
    return new Response(`plugin '${id}' has no install.sh`, { status: 404 });
  }

  const script = readFileSync(file, "utf8");
  return new Response(script, {
    status: 200,
    headers: {
      "content-type": "text/x-shellscript; charset=utf-8",
      // Cache modestly — long enough for a CDN to do its job, short
      // enough that updates land quickly.
      "cache-control": "public, max-age=300, s-maxage=300",
    },
  });
}
