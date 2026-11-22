import { publicCommands } from "@/lib/chat/command-registry";

export const dynamic = "force-static";

/**
 * The prompt commands every chat surface accepts (commands/*.md): `/analyze
 * <question>`, the Agentic Advisor: Finance pack, ... Public, no key. Send one
 * as the start of a user message to POST /v1/chat/completions (or the MCP
 * `chat` tool, `c0mpute chat "/analyze ..."`) and the server expands it.
 */
export function GET() {
  return Response.json(
    { object: "list", data: publicCommands() },
    { headers: { "cache-control": "public, max-age=300", "access-control-allow-origin": "*" } },
  );
}
