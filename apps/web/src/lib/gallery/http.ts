// Small helpers shared by the /api/v1/gallery/* routes.
import { publicStatus, GalleryError } from "./service";

export const GALLERY_CORS = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET, POST, OPTIONS",
  "access-control-allow-headers": "content-type, authorization",
  "access-control-max-age": "86400",
};

export function gjson(body: unknown, status = 200, headers: Record<string, string> = {}) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store", ...GALLERY_CORS, ...headers },
  });
}

export function gerror(e: unknown) {
  if (e instanceof GalleryError) {
    return gjson({ error: { message: e.message, code: e.status }, ...(e.listing ? { listing: publicStatus(e.listing) } : {}) }, e.status);
  }
  console.error("gallery route failed", e);
  return gjson({ error: { message: "something went wrong on our side; try again in a minute", code: 500 } }, 500);
}

export function goptions() {
  return new Response(null, { status: 204, headers: GALLERY_CORS });
}

/** JSON or a plain HTML form post. */
export async function readBody(req: Request): Promise<Record<string, unknown> | null> {
  const type = req.headers.get("content-type") ?? "";
  try {
    if (type.includes("application/json")) return (await req.json()) as Record<string, unknown>;
    if (type.includes("form")) {
      const f = await req.formData();
      const out: Record<string, unknown> = {};
      for (const [k, v] of f.entries()) {
        if (typeof v !== "string") continue;
        if (k === "built_with") out.built_with = [...((out.built_with as string[]) ?? []), v];
        else out[k] = v;
      }
      return out;
    }
    const t = await req.text();
    return t ? (JSON.parse(t) as Record<string, unknown>) : {};
  } catch {
    return null;
  }
}
