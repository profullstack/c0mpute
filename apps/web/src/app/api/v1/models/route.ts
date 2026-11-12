import { listModels } from "@/lib/chat/completions";
import { infernet, json } from "@/lib/chat/server";

export const dynamic = "force-dynamic";

/** OpenAI-shaped model list: what Infernet nodes on the network serve right now. Public. */
export async function GET() {
  try {
    const ids = await listModels(infernet);
    const created = Math.floor(Date.now() / 1000);
    // The model router (DIP 0035) first: it picks among the live models below.
    const auto = ["auto", "auto:fast", "auto:standard", "auto:high", "auto:max"].map((id) => ({ id, object: "model", created, owned_by: "c0mpute-router" }));
    return json({ object: "list", data: [...auto, ...ids.map((id) => ({ id, object: "model", created, owned_by: "c0mpute" }))] });
  } catch (e) {
    return json({ error: { message: e instanceof Error ? e.message : "models unavailable", code: 502 } }, 502);
  }
}
