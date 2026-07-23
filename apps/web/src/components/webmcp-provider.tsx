"use client";

import { useEffect } from "react";

/**
 * WebMCP integration for c0mpute.com.
 *
 * Registers a handful of JavaScript tools on `document.modelContext` so an AI
 * agent visiting the site can query the network and the plugin catalogue
 * directly, rather than scraping the rendered page. See
 * https://webmachinelearning.github.io/webmcp/.
 *
 * The API is an unshipped draft, so everything here is behind a feature-detect
 * and degrades to a no-op in browsers/agents that don't implement it. We support
 * both the current `document.modelContext.registerTool` shape and the older
 * `provideContext({ tools })` shape.
 */

const ORIGIN =
  typeof window !== "undefined" ? window.location.origin : "https://c0mpute.com";

/** Wrap any JSON-serialisable value as an MCP text-content result. */
function json(value: unknown): WebMcpToolResult {
  return { content: [{ type: "text", text: JSON.stringify(value, null, 2) }] };
}

function errorResult(message: string): WebMcpToolResult {
  return { content: [{ type: "text", text: message }], isError: true };
}

async function fetchJson(path: string): Promise<unknown> {
  const res = await fetch(`${ORIGIN}${path}`, {
    headers: { accept: "application/json" },
  });
  if (!res.ok) throw new Error(`${path} → HTTP ${res.status}`);
  return res.json();
}

const TOOLS: WebMcpToolDescriptor[] = [
  {
    name: "c0mpute_list_plugins",
    title: "List c0mpute plugins",
    description:
      "List the plugins/modules available on the c0mpute decentralized compute network (e.g. transcode, coinpay, infernet), including their install commands.",
    inputSchema: { type: "object", properties: {}, additionalProperties: false },
    execute: async () => {
      try {
        return json(await fetchJson("/api/plugins"));
      } catch (e) {
        return errorResult(`Failed to list plugins: ${(e as Error).message}`);
      }
    },
  },
  {
    name: "c0mpute_network_status",
    title: "Get c0mpute network status",
    description:
      "Get live status of the c0mpute network: workers online, jobs in flight, jobs completed in the last 24h, and average job latency per workload type.",
    inputSchema: { type: "object", properties: {}, additionalProperties: false },
    execute: async () => {
      try {
        return json(await fetchJson("/api/status"));
      } catch (e) {
        return errorResult(`Failed to get network status: ${(e as Error).message}`);
      }
    },
  },
  {
    name: "c0mpute_latest_release",
    title: "Get latest c0mpute release",
    description:
      "Get the latest published c0mpute CLI release manifest (version, channel, and per-platform download artifacts).",
    inputSchema: { type: "object", properties: {}, additionalProperties: false },
    execute: async () => {
      try {
        return json(await fetchJson("/releases/latest.json"));
      } catch (e) {
        return errorResult(`Failed to get latest release: ${(e as Error).message}`);
      }
    },
  },
  {
    name: "c0mpute_install_command",
    title: "Get c0mpute install command",
    description:
      "Get the shell command to install the c0mpute CLI, or to install a specific plugin by id. Pass no arguments for the base CLI installer.",
    inputSchema: {
      type: "object",
      properties: {
        plugin: {
          type: "string",
          description:
            "Optional plugin id (e.g. 'transcode', 'coinpay', 'infernet'). Omit to get the base CLI installer.",
        },
      },
      additionalProperties: false,
    },
    execute: (args) => {
      const plugin =
        typeof args.plugin === "string" ? args.plugin.trim() : "";
      if (plugin) {
        return json({
          plugin,
          command: `c0mpute plugin install ${plugin}`,
          install_url: `https://c0mpute.com/plugins/${plugin}/install.sh`,
        });
      }
      return json({
        command: "curl -fsSL https://c0mpute.com/install.sh | sh",
        installs: ["c0mpute", "coinpay", "infernet"],
        note: "Installs three CLIs into ~/.c0mpute/bin.",
      });
    },
  },
];

/** Register all tools; returns a cleanup that unregisters them. */
function register(ctx: ModelContext): () => void {
  // Preferred: per-tool registration returning an unregister handle.
  if (typeof ctx.registerTool === "function") {
    const regs: Array<WebMcpRegistration | void> = [];
    for (const tool of TOOLS) {
      try {
        const r = ctx.registerTool(tool);
        // registerTool may return a promise; we don't await for cleanup, the
        // handle is only needed on unmount which is far later.
        if (r && typeof (r as PromiseLike<unknown>).then === "function") {
          (r as Promise<WebMcpRegistration | void>).then((h) => regs.push(h));
        } else {
          regs.push(r as WebMcpRegistration | void);
        }
      } catch {
        // A single bad registration must not break the others.
      }
    }
    return () => {
      for (const r of regs) {
        try {
          r?.unregister?.();
        } catch {
          /* no-op */
        }
      }
    };
  }

  // Fallback: older bulk API. No documented removal, so cleanup is a no-op.
  if (typeof ctx.provideContext === "function") {
    try {
      ctx.provideContext({ tools: TOOLS });
    } catch {
      /* no-op */
    }
  }
  return () => {};
}

export function WebMcpProvider() {
  useEffect(() => {
    const ctx = document.modelContext ?? navigator.modelContext;
    if (!ctx) return; // Agent/browser doesn't implement WebMCP — no-op.
    return register(ctx);
  }, []);

  return null;
}
