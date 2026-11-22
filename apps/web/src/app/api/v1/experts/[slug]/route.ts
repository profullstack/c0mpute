import { expertBySlug, publicManifest } from "@/lib/experts/catalog";
import { expertPriceUsd, expertsConfig } from "@/lib/experts/config";

export const dynamic = "force-dynamic";

/** GET /api/v1/experts/<slug>: one expert's signed manifest (public, CORS *). */
export async function GET(_req: Request, { params }: { params: Promise<{ slug: string }> }) {
  const { slug } = await params;
  const m = expertBySlug(slug);
  const headers = { "content-type": "application/json", "access-control-allow-origin": "*" };
  if (!m) return new Response(JSON.stringify({ error: { message: `no expert "${slug}"`, code: 404 } }), { status: 404, headers });
  return new Response(JSON.stringify(publicManifest(m, expertPriceUsd(m.pricing.monthly_usd, expertsConfig()))), { headers: { ...headers, "cache-control": "public, max-age=300" } });
}
