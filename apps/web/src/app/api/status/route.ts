import { NextResponse } from "next/server";
import { getStatusPayload } from "@/lib/status";

export async function GET() {
  const payload = await getStatusPayload();
  const maxAge = payload.source === "aggregator" ? 15 : 30;
  return NextResponse.json(payload, {
    headers: { "cache-control": `public, max-age=${maxAge}` },
  });
}
