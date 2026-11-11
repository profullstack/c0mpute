// MCP tools for chat plugins on c0mpute's own MCP server (/api/mcp): list,
// add and remove the remote MCP servers this account's chats can use. The
// `chat` tool then runs with them on (see mcp.ts).
import type { PluginEvent } from "./plugin-shared";
import { publicPlugin, type PluginStoreDb } from "./plugin-store";

export const PLUGIN_TOOLS = [
  {
    name: "chat_plugins_list",
    description:
      "This c0mpute account's chat plugins (remote MCP servers whose tools the `chat` tool's models can use), their status (on, off, needs_sign_in) and tools, plus the catalog of plugins that can be added by name.",
    inputSchema: { type: "object", properties: {} },
    annotations: { title: "List chat plugins", readOnlyHint: true, openWorldHint: false },
  },
  {
    name: "chat_plugins_add",
    description:
      "Add a chat plugin: an https remote MCP server URL, or a catalog name from chat_plugins_list. If the server needs a sign-in, returns `connect_url`: give it to the person to open in a browser (valid 10 minutes); once they sign in, its tools are on in their c0mpute chats.",
    inputSchema: {
      type: "object",
      properties: {
        url: { type: "string", description: "https URL of a remote MCP server (Streamable HTTP)." },
        name: { type: "string", description: "Or a catalog name (see chat_plugins_list)." },
      },
    },
    annotations: { title: "Add a chat plugin", readOnlyHint: false, destructiveHint: false, idempotentHint: true, openWorldHint: true },
  },
  {
    name: "chat_plugins_remove",
    description: "Remove a chat plugin from this account (its sign-in is forgotten).",
    inputSchema: { type: "object", properties: { name: { type: "string", description: "The plugin's slug." } }, required: ["name"] },
    annotations: { title: "Remove a chat plugin", readOnlyHint: false, destructiveHint: true, idempotentHint: true, openWorldHint: false },
  },
] as const;

export const PLUGIN_TOOL_NAMES = new Set<string>(PLUGIN_TOOLS.map((t) => t.name));

export type PluginMcpDeps = {
  store: PluginStoreDb;
  site: string;
  catalog: () => { slug: string; name: string; title: string; description: string; url: string }[];
  add: (userId: string, target: string) => Promise<import("./plugin-store").ChatPlugin>;
  connectLink: (userId: string, slug: string) => string;
};

export async function pluginTool(name: string, args: Record<string, unknown>, userId: string, d: PluginMcpDeps): Promise<{ text: string; isError?: boolean }> {
  if (name === "chat_plugins_list") {
    const plugins = await d.store.list(userId);
    const added = new Set(plugins.map((p) => p.url));
    return {
      text: JSON.stringify({
        plugins: plugins.map((p) => {
          const pub = publicPlugin(p, d.site);
          return { slug: pub.slug, url: pub.url, title: pub.title || pub.name, status: pub.status, tools: pub.tools.map((t) => t.name) };
        }),
        catalog: d.catalog().filter((c) => !added.has(c.url)).map((c) => ({ name: c.slug, title: c.title, description: c.description })),
      }),
    };
  }
  if (name === "chat_plugins_add") {
    const target = typeof args.url === "string" && args.url ? args.url : typeof args.name === "string" ? args.name : "";
    if (!target) return { text: "chat_plugins_add needs `url` (https MCP server) or `name` (catalog)", isError: true };
    const p = await d.add(userId, target);
    const pub = publicPlugin(p, d.site);
    return {
      text: JSON.stringify({
        slug: pub.slug,
        status: pub.status,
        tools: pub.tools.map((t) => t.name),
        ...(pub.connected ? {} : { connect_url: d.connectLink(userId, p.slug), next: "give the person connect_url to open in a browser and sign in" }),
      }),
    };
  }
  if (name === "chat_plugins_remove") {
    const slug = typeof args.name === "string" ? args.name.toLowerCase() : "";
    if (!slug) return { text: "chat_plugins_remove needs `name`", isError: true };
    const gone = await d.store.remove(userId, slug);
    return gone ? { text: JSON.stringify({ removed: slug }) } : { text: `no chat plugin ${slug} on this account`, isError: true };
  }
  return { text: `unknown tool: ${name}`, isError: true };
}

/** A short trace of the plugin steps for an MCP `chat` answer. */
export function describeSteps(events: PluginEvent[]): string {
  const lines: string[] = [];
  for (const e of events) {
    if (e.type === "tool_result") lines.push(`- ${e.plugin} · ${e.tool}: ${e.ok ? "ok" : "failed"}`);
    else if (e.type === "approval_needed") lines.push(`- ${e.plugin} · ${e.tool}: needs the person's approval (in c0mpute.com/chat or \`c0mpute chat\`); not run`);
    else if (e.type === "plugin_error") lines.push(`- ${e.message}`);
  }
  return lines.length ? `\n\n[plugins used]\n${lines.join("\n")}` : "";
}
