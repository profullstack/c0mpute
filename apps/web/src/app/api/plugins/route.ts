/**
 * JSON plugin listing at https://c0mpute.com/api/plugins.
 *
 * The marketplace page (`/plugins`) renders from `loadAllPlugins()` at build
 * time and has no runtime data source. WebMCP tools run client-side, though, so
 * they need a fetchable endpoint to enumerate the plugin catalogue for an agent.
 * This route exposes the same manifest data as compact JSON.
 */

import { NextResponse } from "next/server";
import { loadAllPlugins, tagline, installCommand } from "@/lib/plugins";

export function GET() {
  const plugins = loadAllPlugins().map((p) => ({
    id: p.id,
    name: p.name,
    version: p.version,
    kind: p.kind,
    tagline: tagline(p),
    description: p.description,
    keywords: p.keywords ?? [],
    homepage: p.homepage,
    source: p.source,
    install_command: installCommand(p),
    install_url: `https://c0mpute.com/plugins/${p.id}/install.sh`,
    web: `https://c0mpute.com/plugins/${p.id}`,
  }));

  return NextResponse.json(
    { count: plugins.length, plugins },
    { headers: { "cache-control": "public, max-age=300, s-maxage=300" } },
  );
}
