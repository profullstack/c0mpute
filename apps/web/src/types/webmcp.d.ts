/**
 * Ambient types for the emerging WebMCP API.
 *
 * Spec: https://webmachinelearning.github.io/webmcp/
 *
 * WebMCP lets a web page expose JavaScript "tools" that an AI agent (in the
 * browser or an extension) can discover and invoke, instead of driving the page
 * only through simulated UI. The surface is a draft and not yet in `lib.dom`, so
 * we declare the minimum we depend on here. Everything is optional at runtime —
 * the provider feature-detects before touching it.
 */

/** MCP-style content block returned from a tool's `execute`. */
interface WebMcpTextContent {
  type: "text";
  text: string;
}

interface WebMcpToolResult {
  content: WebMcpTextContent[];
  isError?: boolean;
}

interface WebMcpToolDescriptor {
  name: string;
  /** Human-readable title (optional in the draft spec). */
  title?: string;
  description: string;
  /** JSON Schema for the tool's arguments. */
  inputSchema?: Record<string, unknown>;
  execute: (args: Record<string, unknown>) => Promise<WebMcpToolResult> | WebMcpToolResult;
}

/** Handle returned by `registerTool`, used to remove the tool again. */
interface WebMcpRegistration {
  unregister?: () => void;
}

interface ModelContext {
  registerTool?: (
    tool: WebMcpToolDescriptor,
    options?: Record<string, unknown>,
  ) => WebMcpRegistration | void | Promise<WebMcpRegistration | void>;
  /** Older drafts expose a bulk `provideContext({ tools })` instead. */
  provideContext?: (context: { tools: WebMcpToolDescriptor[] }) => void;
}

interface Document {
  modelContext?: ModelContext;
}

interface Navigator {
  modelContext?: ModelContext;
}
