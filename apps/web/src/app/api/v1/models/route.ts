import { listModels } from "@/lib/chat/completions";
import { infernet, json } from "@/lib/chat/server";
import { supportsTools } from "@/lib/chat/tools";
import { experts } from "@/lib/experts/catalog";

export const dynamic = "force-dynamic";

/**
 * OpenAI-shaped model list: what Infernet nodes on the network serve right now. Public.
 * `capabilities.tool_call`: the model takes `tools` on /v1/chat/completions (lib/chat/tools.ts).
 * `expert:<slug>`: the c0mpute Experts (subscription after a free trial; GET /api/v1/experts).
 */
export async function GET() {
  try {
    const ids = await listModels(infernet);
    const created = Math.floor(Date.now() / 1000);
    // The model router (DIP 0035) first: it picks among the live models below.
    const anyTools = ids.some(supportsTools);
    const auto = ["auto", "auto:fast", "auto:standard", "auto:high", "auto:max"].map((id) => ({
      id,
      object: "model",
      created,
      owned_by: "c0mpute-router",
      capabilities: { tool_call: anyTools },
    }));
    return json({
      object: "list",
      data: [...auto, ...experts().map((m) => ({ id: `expert:${m.slug}`, object: "model", created, owned_by: "c0mpute-experts", name: m.name, capabilities: { tool_call: false } })), ...ids.map((id) => ({ id, object: "model", created, owned_by: "c0mpute", capabilities: { tool_call: supportsTools(id) } }))],
    });
  } catch (e) {
    return json({ error: { message: e instanceof Error ? e.message : "models unavailable", code: 502 } }, 502);
  }
}
