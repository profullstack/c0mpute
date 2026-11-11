// The local MCP client: connects to the enabled servers from ~/mcps over
// stdio, streamable HTTP or SSE (the official SDK), lists their tools and
// calls them for the Agent chat.

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { SSEClientTransport } from "@modelcontextprotocol/sdk/client/sse.js";
import { StdioClientTransport, getDefaultEnvironment } from "@modelcontextprotocol/sdk/client/stdio.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import { expandServer, type McpServer } from "./mcp-config";

export interface McpTool {
  server: string;
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
}

export interface McpConn {
  server: McpServer;
  client: Client | null;
  tools: McpTool[];
  state: "connecting" | "ready" | "error" | "off";
  error?: string;
  stderr: string[];
}

const VERSION = "0.3.28";

/** Tool name the model sees: `<server>__<tool>`, limited to [A-Za-z0-9_-]. */
export function qualifiedName(server: string, tool: string): string {
  return `${server}__${tool}`.replace(/[^A-Za-z0-9_-]/g, "_").slice(0, 128);
}

/** MCP tool result content -> text for the model. */
export function resultText(result: unknown): { text: string; isError: boolean } {
  const r = result as { content?: { type: string; text?: string; mimeType?: string; resource?: { uri?: string; text?: string } }[]; isError?: boolean; structuredContent?: unknown };
  const parts: string[] = [];
  for (const c of r?.content ?? []) {
    if (c.type === "text" && typeof c.text === "string") parts.push(c.text);
    else if (c.type === "resource" && c.resource) parts.push(c.resource.text ?? `[resource ${c.resource.uri ?? ""}]`);
    else parts.push(`[${c.type}${c.mimeType ? ` ${c.mimeType}` : ""} content not shown]`);
  }
  if (!parts.length && r?.structuredContent !== undefined) parts.push(JSON.stringify(r.structuredContent, null, 2));
  return { text: parts.join("\n") || "(no content)", isError: r?.isError === true };
}

export class McpManager {
  conns = new Map<string, McpConn>();
  constructor(private lookup: (name: string) => string | undefined) {}

  private transport(s: McpServer, conn: McpConn): Transport {
    const { server, missing } = expandServer(s, this.lookup);
    if (missing.length) throw new Error(`set ${missing.join(", ")} (environment or the app's secret store)`);
    if (server.transport === "stdio") {
      const t = new StdioClientTransport({
        command: server.command!,
        args: server.args ?? [],
        env: { ...getDefaultEnvironment(), PATH: process.env.PATH ?? "", HOME: process.env.HOME ?? "", ...(server.env ?? {}) },
        cwd: server.cwd,
        stderr: "pipe",
      });
      t.stderr?.on("data", (d: Buffer) => {
        conn.stderr.push(...d.toString().split(/\r?\n/).filter(Boolean));
        if (conn.stderr.length > 200) conn.stderr.splice(0, conn.stderr.length - 200);
      });
      return t;
    }
    const url = new URL(server.url!);
    const requestInit = { headers: server.headers ?? {} };
    return server.transport === "sse" ? new SSEClientTransport(url, { requestInit }) : new StreamableHTTPClientTransport(url, { requestInit });
  }

  async connect(s: McpServer): Promise<McpConn> {
    await this.disconnect(s.name);
    const conn: McpConn = { server: s, client: null, tools: [], state: s.disabled ? "off" : "connecting", stderr: [] };
    this.conns.set(s.name, conn);
    if (s.disabled) return conn;
    try {
      const client = new Client({ name: "c0mpute-desktop", version: VERSION });
      let transport = this.transport(s, conn);
      try {
        await withTimeout(client.connect(transport), 30_000, "connect");
      } catch (e) {
        // Older remote servers speak only SSE. When that fails too, the
        // streamable-HTTP error is the one worth showing (a 401, say).
        if (s.transport !== "http") throw e;
        transport = this.transport({ ...s, transport: "sse" }, conn);
        try {
          await withTimeout(client.connect(transport), 30_000, "connect (sse)");
        } catch {
          throw e;
        }
      }
      conn.client = client;
      const list = await withTimeout(client.listTools(), 30_000, "tools/list");
      conn.tools = list.tools.map((t) => ({
        server: s.name,
        name: t.name,
        description: t.description ?? "",
        inputSchema: (t.inputSchema as Record<string, unknown>) ?? { type: "object" },
      }));
      conn.state = "ready";
    } catch (e) {
      conn.state = "error";
      conn.error = e instanceof Error ? e.message : String(e);
    }
    return conn;
  }

  async disconnect(name: string) {
    const c = this.conns.get(name);
    this.conns.delete(name);
    await c?.client?.close().catch(() => {});
  }

  async closeAll() {
    await Promise.all([...this.conns.keys()].map((n) => this.disconnect(n)));
  }

  /** Bring connections in line with the server list (connects in parallel). */
  async sync(servers: McpServer[]) {
    const want = new Map(servers.map((s) => [s.name, s]));
    for (const name of [...this.conns.keys()]) if (!want.has(name)) await this.disconnect(name);
    await Promise.all(
      servers.map((s) => {
        const c = this.conns.get(s.name);
        const same = c && JSON.stringify(c.server) === JSON.stringify(s) && c.state !== "error";
        return same ? Promise.resolve(c) : this.connect(s);
      }),
    );
  }

  tools(): McpTool[] {
    return [...this.conns.values()].filter((c) => c.state === "ready").flatMap((c) => c.tools);
  }

  async call(server: string, tool: string, args: Record<string, unknown>, signal?: AbortSignal): Promise<{ text: string; isError: boolean }> {
    const c = this.conns.get(server);
    if (!c?.client || c.state !== "ready") throw new Error(`MCP server ${server} is not connected${c?.error ? `: ${c.error}` : ""}`);
    const r = await c.client.callTool({ name: tool, arguments: args }, undefined, { signal, timeout: 300_000 });
    return resultText(r);
  }
}

function withTimeout<T>(p: Promise<T>, ms: number, what: string): Promise<T> {
  let t: NodeJS.Timeout;
  return Promise.race([
    p.finally(() => clearTimeout(t)),
    new Promise<never>((_, rej) => {
      t = setTimeout(() => rej(new Error(`${what} timed out after ${ms / 1000} s`)), ms);
    }),
  ]);
}
