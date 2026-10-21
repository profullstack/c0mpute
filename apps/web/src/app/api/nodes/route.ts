import { NextResponse } from "next/server";
import { getNodesPayload } from "@/lib/nodes";

export async function GET() {
  const payload = await getNodesPayload();
  return NextResponse.json(payload, {
    headers: {
      "cache-control": "public, max-age=30",
      "access-control-allow-origin": "*",
    },
  });
}
