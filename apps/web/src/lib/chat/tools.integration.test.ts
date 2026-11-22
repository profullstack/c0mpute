// A real tool round-trip against a node's Ollama, through the same translation
// /v1/chat/completions uses (lib/chat/tools.ts). Skipped unless OLLAMA_URL is set:
//
//   OLLAMA_URL=http://127.0.0.1:11434 TOOL_MODELS=qwen3:4b,qwen2.5-coder:7b \
//     bun test src/lib/chat/tools.integration.test.ts
//
// Step 1 must come back as a get_weather tool call with {"city": "Paris"};
// step 2 sends the tool result and must come back as a plain answer that uses it.

import { describe, expect, test } from "bun:test";
import { C0_SYSTEM_PROMPT, parseChatRequest, type ChatMessage, type ChatRequest } from "./completions";
import { applyChoice, parseToolReply, withToolsPrompt } from "./tools";

const OLLAMA = process.env.OLLAMA_URL ?? "";
const MODELS = (process.env.TOOL_MODELS ?? "qwen3:4b").split(",").map((s) => s.trim()).filter(Boolean);

async function ollama(model: string, messages: ChatMessage[]): Promise<string> {
  const res = await fetch(`${OLLAMA}/api/chat`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ model, messages, stream: false, options: { temperature: 0 } }),
    signal: AbortSignal.timeout(600_000),
  });
  if (!res.ok) throw new Error(`ollama HTTP ${res.status}: ${await res.text()}`);
  return ((await res.json()) as { message: { content: string } }).message.content;
}

const weather = {
  type: "function",
  function: {
    name: "get_weather",
    description: "Get the current weather for a city",
    parameters: { type: "object", properties: { city: { type: "string" }, unit: { type: "string", enum: ["c", "f"] } }, required: ["city"] },
  },
};

describe.skipIf(!OLLAMA)("real tool round-trip on a node", () => {
  for (const model of MODELS) {
    test(
      model,
      async () => {
        const user = { role: "user", content: "What's the weather in Paris right now? Use the tool." };
        const req1 = parseChatRequest({ model, tools: [weather], messages: [user] }) as ChatRequest;
        const raw1 = await ollama(model, withToolsPrompt(req1.messages, req1.tooling!, C0_SYSTEM_PROMPT));
        const step1 = applyChoice(parseToolReply(raw1, req1.tooling!.tools), req1.tooling!);
        console.log(`[${model}] step 1 raw: ${JSON.stringify(raw1.slice(0, 300))}`);
        expect(step1.calls.length).toBeGreaterThanOrEqual(1);
        const call = step1.calls[0];
        expect(call.function.name).toBe("get_weather");
        expect(String((JSON.parse(call.function.arguments) as { city: string }).city).toLowerCase()).toContain("paris");

        const req2 = parseChatRequest({
          model,
          tools: [weather],
          messages: [
            user,
            { role: "assistant", content: step1.content, tool_calls: step1.calls },
            { role: "tool", tool_call_id: call.id, content: JSON.stringify({ city: "Paris", temp_c: 17, conditions: "light rain" }) },
          ],
        }) as ChatRequest;
        const raw2 = await ollama(model, withToolsPrompt(req2.messages, req2.tooling!, C0_SYSTEM_PROMPT));
        const step2 = applyChoice(parseToolReply(raw2, req2.tooling!.tools), req2.tooling!);
        console.log(`[${model}] step 2 raw: ${JSON.stringify(raw2.slice(0, 300))}`);
        expect(step2.calls).toHaveLength(0);
        expect(step2.content ?? "").toMatch(/17|rain/i);
      },
      1_200_000,
    );
  }
});
