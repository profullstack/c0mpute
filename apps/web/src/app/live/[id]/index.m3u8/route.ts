import { liveStore } from "@/lib/chat/server";
import { liveGatewayUrl } from "@/lib/live/store";

export const dynamic = "force-dynamic";

/**
 * GET /live/<id>/index.m3u8: the stream's HLS playlist, for any player (VLC,
 * mpv, ffplay, Safari, hls.js). A redirect to the gateway playlist of the
 * network stream it is on air under; 404 while it is off air.
 */
export async function GET(_req: Request, ctx: { params: Promise<{ id: string }> }) {
  const s = await liveStore()
    .publicGet((await ctx.params).id)
    .catch(() => null);
  if (!s || !s.live || !s.network_id) {
    return new Response("#EXTM3U\n", { status: 404, headers: { "content-type": "application/vnd.apple.mpegurl", "cache-control": "no-store", "access-control-allow-origin": "*" } });
  }
  return new Response(null, { status: 302, headers: { location: `${liveGatewayUrl()}/live/${s.network_id}/index.m3u8`, "cache-control": "no-store", "access-control-allow-origin": "*" } });
}
