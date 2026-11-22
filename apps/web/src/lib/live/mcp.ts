// c0mpute Live MCP tools (live_stream_create, live_streams, live_stream_end)
// for /api/mcp. The store is passed in; nothing here reads the environment.
import { LIVE_RTMP_URL, LIVE_USD_PER_GB, LiveError, streamJson, type LiveStore } from "./store";

export interface LiveMcpDeps {
  store: LiveStore;
  site: string;
}

export const LIVE_TOOLS = [
  {
    name: "live_stream_create",
    description: `Create a live stream on c0mpute Live. Returns the RTMP server and stream key to put in OBS or PairUX (the key is shown once), and the watch link to share. Nodes relay the stream peer to peer; relays are paid $${LIVE_USD_PER_GB} per GB from the person's balance, or it runs on the free tier (720p, 25 viewers) when the balance is empty.`,
    inputSchema: {
      type: "object",
      properties: {
        name: { type: "string", description: "Short name, e.g. \"friday show\". Unique on the account." },
        title: { type: "string", description: "Title shown on the watch page." },
        visibility: { type: "string", enum: ["public", "unlisted"], description: "unlisted: not on c0mpute.com/live, link only. Default public." },
      },
      required: ["name"],
    },
    annotations: { title: "Create a live stream", readOnlyHint: false, destructiveHint: false, openWorldHint: false },
  },
  {
    name: "live_streams",
    description: "The person's live streams: watch link, playlist link, whether each is on air, and what relays have carried (GB and cost).",
    inputSchema: { type: "object", properties: {} },
    annotations: { title: "My live streams", readOnlyHint: true, openWorldHint: false },
  },
  {
    name: "live_stream_end",
    description: "Delete a live stream: its key and watch link stop working at once. Ask the person first.",
    inputSchema: { type: "object", properties: { id: { type: "string", description: "The stream id (lv_...)." } }, required: ["id"] },
    annotations: { title: "Delete a live stream", readOnlyHint: false, destructiveHint: true, openWorldHint: false },
  },
] as const;
export const LIVE_TOOL_NAMES = new Set<string>(LIVE_TOOLS.map((t) => t.name));

export async function liveTool(
  name: string,
  args: Record<string, unknown>,
  user: { id: string; coinpay_sub: string | null },
  deps: LiveMcpDeps,
): Promise<{ text: string; isError?: boolean }> {
  try {
    if (name === "live_stream_create") {
      const { stream, stream_key } = await deps.store.create(user, args);
      return { text: JSON.stringify({ ...streamJson(stream, deps.site), stream_key, publish_url: `${LIVE_RTMP_URL}/${stream_key}`, note: "The stream key is shown once. Push RTMP to publish_url; share watch_url." }, null, 2) };
    }
    if (name === "live_streams") {
      const streams = await deps.store.list(user.id);
      const out = await Promise.all(streams.map(async (s) => ({ ...streamJson(s, deps.site), usage: await deps.store.usage(s.id) })));
      return { text: JSON.stringify({ streams: out }, null, 2) };
    }
    if (name === "live_stream_end") {
      const ok = await deps.store.remove(user.id, String(args.id ?? ""));
      return ok ? { text: "deleted" } : { text: "no such stream on this account", isError: true };
    }
    return { text: `unknown tool: ${name}`, isError: true };
  } catch (e) {
    if (e instanceof LiveError) return { text: e.message, isError: true };
    throw e;
  }
}
