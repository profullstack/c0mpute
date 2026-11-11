// A tiny stdio MCP server for the round-trip test: `echo` and `add`.
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";

const server = new Server({ name: "echo-fixture", version: "1.0.0" }, { capabilities: { tools: {} } });

server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: [
    { name: "echo", description: "Echo text back", inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] } },
    { name: "add", description: "Add two numbers", inputSchema: { type: "object", properties: { a: { type: "number" }, b: { type: "number" } }, required: ["a", "b"] } },
    { name: "secret", description: "Says whether FIXTURE_TOKEN reached the server env", inputSchema: { type: "object", properties: {} } },
  ],
}));

server.setRequestHandler(CallToolRequestSchema, async (req) => {
  const a = (req.params.arguments ?? {}) as Record<string, unknown>;
  switch (req.params.name) {
    case "echo":
      return { content: [{ type: "text", text: `echo: ${a.text}` }] };
    case "add":
      return { content: [{ type: "text", text: String(Number(a.a) + Number(a.b)) }] };
    case "secret":
      return { content: [{ type: "text", text: process.env.FIXTURE_TOKEN ? `token length ${process.env.FIXTURE_TOKEN.length}` : "no token" }] };
    default:
      return { content: [{ type: "text", text: `unknown tool ${req.params.name}` }], isError: true };
  }
});

await server.connect(new StdioServerTransport());
