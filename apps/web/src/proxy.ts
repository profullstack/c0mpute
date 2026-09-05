import { gate } from "@/lib/crawl-gateway";
import { NextResponse, type NextRequest } from "next/server";

export async function proxy(request: NextRequest) {
  // Crawl gateway first: AI training crawlers get 402 Payment Required (or the
  // sales page at /crawl) unless they present a paid pass. People, Googlebot
  // and retrieval crawlers fall through to everything below.
  const answer = await gate(request);
  if (answer) return answer;

  return NextResponse.next();
}

export const config = {
  // Everything but Next's own assets and static files. API routes stay
  // covered on purpose: a training crawler hitting the API gets 402 too.
  matcher: [
    "/((?!_next/static|_next/image|favicon.ico|.*\.(?:svg|png|jpg|jpeg|gif|webp|ico|css|js|map|woff|woff2|ttf|otf|mp3|mp4|webmanifest)$).*)",
  ],
};
