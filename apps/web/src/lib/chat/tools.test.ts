import { describe, expect, test } from "bun:test";
import { C0_SYSTEM_PROMPT, forwardChat, parseChatRequest, type ChatRequest, type Upstream } from "./completions";
import {
  applyChoice,
  autoToolModels,
  contentText,
  needsRepair,
  parseTooling,
  parseToolReply,
  renderMessages,
  supportsTools,
  toolCompletionJson,
  toolStreamChunks,
  withToolsPrompt,
  type Tooling,
} from "./tools";
import { runTools, toolChatResponse, toolModels, ToolChatError } from "./tools-http";

const weather = {
  type: "function",
  function: {
    name: "get_weather",
    description: "Get the current weather for a city",
    parameters: { type: "object", properties: { city: { type: "string" } }, required: ["city"] },
  },
};
const time = { type: "function", function: { name: "get_time", parameters: { type: "object", properties: {} } } };
const T = (o: Partial<Tooling> = {}): Tooling => ({
  tools: [
    { name: "get_weather", parameters: weather.function.parameters },
    { name: "get_time", parameters: { type: "object", properties: {} } },
  ],
  choice: "auto",
  parallel: true,
  ...o,
});

describe("request validation", () => {
  test("tools, tool_choice, parallel_tool_calls", () => {
    expect(parseTooling({})).toBeNull();
    expect(parseTooling({ tools: [] })).toBeNull();
    expect(parseTooling({ tool_choice: "required" })).toBe("tool_choice needs tools[]");
    expect(parseTooling({ tools: "x" })).toBe("tools must be an array");
    expect(parseTooling({ tools: [{ type: "retrieval" }] })).toMatch(/type: "function"/);
    expect(parseTooling({ tools: [{ type: "function", function: { name: "bad name" } }] })).toMatch(/name must match/);
    expect(parseTooling({ tools: [weather, weather] })).toMatch(/duplicate/);
    expect(parseTooling({ tools: [{ type: "function", function: { name: "x", parameters: [] } }] })).toMatch(/JSON Schema/);
    expect(parseTooling({ tools: [weather], tool_choice: { type: "function", function: { name: "nope" } } })).toMatch(/must name one of/);
    expect(parseTooling({ tools: [weather], tool_choice: "always" })).toMatch(/tool_choice must be/);
    const ok = parseTooling({ tools: [weather, time], tool_choice: { type: "function", function: { name: "get_time" } }, parallel_tool_calls: false }) as Tooling;
    expect(ok.choice).toEqual({ name: "get_time" });
    expect(ok.parallel).toBe(false);
    expect(ok.tools[1].parameters).toEqual({ type: "object", properties: {} });
    expect((parseTooling({ tools: [weather] }) as Tooling).choice).toBe("auto");
  });

  test("content parts: text flattened, images refused with a clear message", () => {
    expect(contentText([{ type: "text", text: "a" }, { type: "text", text: "b" }], "user")).toBe("a\nb");
    expect(contentText(null, "assistant")).toBe("");
    expect(contentText([{ type: "image_url", image_url: { url: "x" } }], "user")).toEqual({ error: expect.stringMatching(/image input is not supported/) });
    expect(contentText(42, "user")).toEqual({ error: expect.stringMatching(/string or an array/) });
    const r = parseChatRequest({ messages: [{ role: "user", content: [{ type: "text", text: "hi" }] }] });
    expect(r).toEqual({ messages: [{ role: "user", content: "hi" }], stream: false });
    expect(parseChatRequest({ messages: [{ role: "user", content: [{ type: "image_url", image_url: { url: "x" } }] }] })).toMatch(/image input/);
  });

  test("tool_choice none: no tooling, history still rendered", () => {
    const r = parseChatRequest({ messages: [{ role: "user", content: "hi" }], tools: [weather], tool_choice: "none" }) as ChatRequest;
    expect(r.tooling).toBeUndefined();
    const w = parseChatRequest({ messages: [{ role: "user", content: "hi" }], tools: [weather], stream: true, stream_options: { include_usage: true } }) as ChatRequest;
    expect(w.tooling?.tools[0].name).toBe("get_weather");
    expect(w.include_usage).toBe(true);
  });

  test("response_format json goes into the system prompt", () => {
    const r = parseChatRequest({ messages: [{ role: "user", content: "hi" }], response_format: { type: "json_object" } }) as ChatRequest;
    expect(r.messages[0].role).toBe("system");
    expect(r.messages[0].content.startsWith(C0_SYSTEM_PROMPT)).toBe(true);
    expect(r.messages[0].content).toMatch(/one valid JSON object/);
    const own = parseChatRequest({ messages: [{ role: "system", content: "be terse" }, { role: "user", content: "hi" }], response_format: { type: "json_object" } }) as ChatRequest;
    expect(own.messages).toHaveLength(2);
    expect(own.messages[0].content).toMatch(/^be terse\n\nRespond with one valid JSON/);
    expect(parseChatRequest({ messages: [{ role: "user", content: "hi" }], response_format: { type: "yaml" } })).toMatch(/response_format.type/);
    expect(parseChatRequest({ messages: [{ role: "user", content: "hi" }], response_format: { type: "text" } })).toEqual({ messages: [{ role: "user", content: "hi" }], stream: false });
  });
});

describe("OpenAI -> model translation", () => {
  test("assistant tool_calls and tool results become <tool_call> / <tool_response>", () => {
    const r = renderMessages([
      { role: "developer", content: "sys" },
      { role: "user", content: "weather in Paris and the time?" },
      {
        role: "assistant",
        content: null,
        tool_calls: [
          { id: "call_1", type: "function", function: { name: "get_weather", arguments: '{"city":"Paris"}' } },
          { id: "call_2", type: "function", function: { name: "get_time", arguments: "{}" } },
        ],
      },
      { role: "tool", tool_call_id: "call_1", content: '{"temp_c":18}' },
      { role: "tool", tool_call_id: "call_2", content: "12:00" },
    ]);
    expect(r).toEqual([
      { role: "system", content: "sys" },
      { role: "user", content: "weather in Paris and the time?" },
      {
        role: "assistant",
        content: '<tool_call>\n{"name":"get_weather","arguments":{"city":"Paris"}}\n</tool_call>\n<tool_call>\n{"name":"get_time","arguments":{}}\n</tool_call>',
      },
      {
        role: "user",
        content: '<tool_response>\n{"name":"get_weather","content":{"temp_c":18}}\n</tool_response>\n<tool_response>\n{"name":"get_time","content":"12:00"}\n</tool_response>',
      },
    ]);
    expect(renderMessages([{ role: "assistant", tool_calls: "x" }])).toMatch(/must be an array/);
    expect(renderMessages([{ role: "tool", tool_call_id: 5, content: "x" }])).toMatch(/tool_call_id/);
    expect(renderMessages([{ role: "wizard", content: "x" }])).toMatch(/role must be/);
  });

  test("tools prompt follows tool_choice", () => {
    const base = [{ role: "user" as const, content: "hi" }];
    const auto = withToolsPrompt(base, T(), "HOUSE");
    expect(auto[0].role).toBe("system");
    expect(auto[0].content).toMatch(/^HOUSE\n\n# Tools/);
    expect(auto[0].content).toContain('"name":"get_weather"');
    expect(auto[0].content).toMatch(/reply to the user directly/);
    expect(withToolsPrompt(base, T({ choice: "required" }), "H")[0].content).toMatch(/must call at least one/);
    const named = withToolsPrompt(base, T({ choice: { name: "get_time" } }), "H")[0].content;
    expect(named).toMatch(/must call the function "get_time"/);
    expect(named).not.toContain("get_weather");
    expect(withToolsPrompt(base, T({ parallel: false }), "H")[0].content).toMatch(/at most one function/);
    const own = withToolsPrompt([{ role: "system", content: "mine" }, ...base], T(), "H");
    expect(own).toHaveLength(2);
    expect(own[0].content).toMatch(/^mine\n\n# Tools/);
  });
});

describe("model -> OpenAI translation", () => {
  const tools = T().tools;
  test("<tool_call> JSON blocks (qwen3), with think blocks and stray closing tags", () => {
    const p = parseToolReply('<think>need weather</think>\n<tool_call>\n{"name": "get_weather", "arguments": {"city": "Paris", "unit": "c"}}\n</tool_call></tool_call>', tools);
    expect(p.content).toBeNull();
    expect(p.calls).toHaveLength(1);
    expect(p.calls[0].id).toMatch(/^call_[A-Za-z0-9]{24}$/);
    expect(p.calls[0].type).toBe("function");
    expect(p.calls[0].function).toEqual({ name: "get_weather", arguments: '{"city":"Paris","unit":"c"}' });
  });
  test("unterminated block (nemotron) and two calls", () => {
    const p = parseToolReply('<tool_call>\n{"name": "get_weather", "arguments": {"city": "Paris"}}\n</tool_call>\n<tool_call>\n{"name": "get_time", "arguments": {}}', tools);
    expect(p.calls.map((c) => c.function.name)).toEqual(["get_weather", "get_time"]);
  });
  test("<json> tags (qwen2.5-coder) and bare JSON", () => {
    expect(parseToolReply('<json>\n  {"name": "get_weather", "arguments": {"city": "Paris"}}\n</json></json>', tools).calls[0].function.name).toBe("get_weather");
    const bare = parseToolReply('{"name": "get_weather", "arguments": {"city": "Paris"}}', tools);
    expect(bare.calls).toHaveLength(1);
    expect(bare.content).toBeNull();
    expect(parseToolReply('```json\n{"name":"get_weather","parameters":{"city":"Oslo"}}\n```', tools).calls[0].function.arguments).toBe('{"city":"Oslo"}');
  });
  test("<function=...><parameter=...> form", () => {
    const p = parseToolReply("<tool_call>\n<function=get_weather>\n<parameter=city>\nParis\n</parameter>\n</function>\n</tool_call>", tools);
    expect(p.calls[0].function).toEqual({ name: "get_weather", arguments: '{"city":"Paris"}' });
  });
  test("loose JSON repaired: trailing comma, single quotes, string arguments", () => {
    expect(parseToolReply('<tool_call>{"name": "get_weather", "arguments": {"city": "Paris",},}</tool_call>', tools).calls).toHaveLength(1);
    expect(parseToolReply("<tool_call>{'name': 'get_weather', 'arguments': {'city': 'Rome'}}</tool_call>", tools).calls[0].function.arguments).toBe('{"city":"Rome"}');
    expect(parseToolReply('<tool_call>{"name": "get_weather", "arguments": "{\\"city\\": \\"Rome\\"}"}</tool_call>', tools).calls[0].function.arguments).toBe('{"city":"Rome"}');
  });
  test("plain answers stay content; unknown tools and plain JSON are not calls", () => {
    const p = parseToolReply("It is 18 C and sunny in Paris.", tools);
    expect(p).toEqual({ content: "It is 18 C and sunny in Paris.", calls: [], attempted: false, maybe: false });
    expect(parseToolReply('{"name": "launch_missiles", "arguments": {}}', tools).calls).toHaveLength(0);
    expect(parseToolReply('{"name": "Paris", "temp": 18}', tools).calls).toHaveLength(0);
    const mixed = parseToolReply('Let me check.\n<tool_call>{"name":"get_time","arguments":{}}</tool_call>', tools);
    expect(mixed.content).toBe("Let me check.");
    expect(mixed.calls).toHaveLength(1);
  });
  test("unreadable call is flagged for repair", () => {
    const bad = parseToolReply("<tool_call>get_weather(city=Paris)</tool_call>", tools);
    expect(bad.calls).toHaveLength(0);
    expect(bad.attempted).toBe(true);
    expect(needsRepair(bad, T())).toBe(true);
    expect(needsRepair(parseToolReply("hello", tools), T())).toBe(false);
    expect(needsRepair(parseToolReply("hello", tools), T({ choice: "required" }))).toBe(true);
  });
  test("applyChoice: named function and parallel_tool_calls false", () => {
    const p = parseToolReply('<tool_call>{"name":"get_weather","arguments":{"city":"a"}}</tool_call><tool_call>{"name":"get_time","arguments":{}}</tool_call>', tools);
    expect(applyChoice(p, T({ choice: { name: "get_time" } })).calls.map((c) => c.function.name)).toEqual(["get_time"]);
    expect(applyChoice(p, T({ parallel: false })).calls).toHaveLength(1);
  });

  test("completion JSON and stream chunks follow the OpenAI shape", () => {
    const p = parseToolReply('<tool_call>{"name":"get_weather","arguments":{"city":"Paris, the capital of France, in Europe"}}</tool_call>', tools);
    const usage = { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 };
    const j = toolCompletionJson(p, "qwen3:4b", usage, "chatcmpl-x");
    expect(j.choices[0].finish_reason).toBe("tool_calls");
    expect(j.choices[0].message.content).toBeNull();
    expect(j.choices[0].message.tool_calls?.[0].function.name).toBe("get_weather");
    expect(JSON.parse(j.choices[0].message.tool_calls![0].function.arguments)).toEqual({ city: "Paris, the capital of France, in Europe" });
    expect(j.usage).toEqual(usage);

    const chunks = toolStreamChunks(p, "qwen3:4b", usage, "chatcmpl-x") as {
      choices: { delta: { role?: string; content?: string | null; tool_calls?: { index: number; id?: string; type?: string; function: { name?: string; arguments: string } }[] }; finish_reason: string | null }[];
      usage?: unknown;
    }[];
    expect(chunks[0].choices[0].delta).toEqual({ role: "assistant", content: null });
    const first = chunks[1].choices[0].delta.tool_calls![0];
    expect(first).toEqual({ index: 0, id: p.calls[0].id, type: "function", function: { name: "get_weather", arguments: "" } });
    // Argument pieces reassemble to the JSON string; later pieces carry no id or name.
    let args = "";
    for (const c of chunks.slice(2, -2)) {
      const tc = c.choices[0].delta.tool_calls![0];
      expect(tc.index).toBe(0);
      expect(tc.id).toBeUndefined();
      args += tc.function.arguments;
    }
    expect(chunks.slice(2, -2).length).toBeGreaterThan(1);
    expect(args).toBe(p.calls[0].function.arguments);
    expect(chunks[chunks.length - 2].choices[0].finish_reason).toBe("tool_calls");
    expect(chunks[chunks.length - 1]).toMatchObject({ choices: [], usage });

    const text = toolStreamChunks(parseToolReply("Sunny.", tools), "m", null) as { choices: { delta: Record<string, unknown>; finish_reason: string | null }[] }[];
    expect(text.map((c) => c.choices[0].delta)).toEqual([{ role: "assistant", content: "" }, { content: "Sunny." }, {}]);
    expect(text[2].choices[0].finish_reason).toBe("stop");
  });
});

describe("which models take tools", () => {
  test("allowlist", () => {
    for (const m of ["qwen3:4b", "qwen2.5-coder:7b", "qwen3-coder:30b", "nemotron-3-nano:4b", "huihui_ai/qwen3.5-abliterated:4b", "devstral:24b", "llama3.1:8b", "qwen2.5:7b"]) expect(supportsTools(m)).toBe(true);
    for (const m of ["gemma3:4b", "deepseek-coder-v2:16b", "qwen2.5:0.5b", "qwen2.5-coder:1.5b", "huihui_ai/qwen3.5-abliterated:0.8b", "openbmb/minicpm-v4.7:1b", "hf.co/profullstack/tax-guy-agent-GGUF:latest"]) expect(supportsTools(m)).toBe(false);
  });
  test("auto order: live, preferred first, never uncensored", () => {
    const live = ["gemma3:4b", "qwen2.5-coder:7b", "qwen3:4b", "huihui_ai/qwen3.5-abliterated:4b", "nemotron-3-nano:4b", "qwen3-coder:30b"];
    expect(autoToolModels(live, "standard")).toEqual(["qwen3:4b", "nemotron-3-nano:4b", "qwen2.5-coder:7b", "qwen3-coder:30b"]);
    expect(autoToolModels(live, "max")[0]).toBe("qwen3-coder:30b");
    expect(autoToolModels(["gemma3:4b"], "fast")).toEqual([]);
  });
});

// ── the HTTP path against a fake Infernet ────────────────────────────────

type Reply = { status?: number; text?: string; error?: string };
function fakeInfernet(replies: Record<string, Reply[]>, models = ["qwen3:4b", "nemotron-3-nano:4b", "qwen2.5-coder:7b", "gemma3:4b"]) {
  const calls: { model: string; messages: { role: string; content: string }[]; body: Record<string, unknown> }[] = [];
  const up: Upstream = async (path, init) => {
    if (path === "/v1/models") return Response.json({ data: models.map((id) => ({ id })) });
    const body = JSON.parse(String(init?.body));
    calls.push({ model: body.model, messages: body.messages, body });
    const r = replies[body.model]?.shift() ?? { status: 503, error: "no live node" };
    if (r.error) return Response.json({ error: { message: r.error } }, { status: r.status ?? 502 });
    return Response.json({ model: body.model, choices: [{ message: { role: "assistant", content: r.text } }] });
  };
  return { up, calls };
}

const toolReq = (extra: Record<string, unknown> = {}) =>
  parseChatRequest({ messages: [{ role: "user", content: "weather in Paris?" }], tools: [weather], ...extra }) as ChatRequest;

describe("tool requests end to end (fake Infernet)", () => {
  test("named non-tool model: 400 listing the tool-capable ones", async () => {
    const { up } = fakeInfernet({});
    const res = await toolChatResponse({ upstream: up, req: toolReq({ model: "gemma3:4b" }), rawModel: "gemma3:4b" });
    expect(res.status).toBe(400);
    const j = (await res.json()) as { error: { message: string } };
    expect(j.error.message).toMatch(/does not support tool calling/);
    expect(j.error.message).toContain("qwen3:4b");
  });

  test("auto: tools reach the model in the system prompt, not as a dropped field; busy model hands over", async () => {
    const { up, calls } = fakeInfernet({
      "qwen3:4b": [{ status: 502, error: "Network is busy. Run a node or wait." }],
      "nemotron-3-nano:4b": [{ text: '<tool_call>\n{"name": "get_weather", "arguments": {"city": "Paris"}}' }],
    });
    const res = await toolChatResponse({ upstream: up, req: toolReq({ model: "auto" }), rawModel: "auto" });
    expect(res.status).toBe(200);
    const j = (await res.json()) as ReturnType<typeof toolCompletionJson>;
    expect(j.model).toBe("nemotron-3-nano:4b");
    expect(j.choices[0].finish_reason).toBe("tool_calls");
    expect(j.choices[0].message.tool_calls?.[0].function).toEqual({ name: "get_weather", arguments: '{"city":"Paris"}' });
    expect(j.usage.total_tokens).toBeGreaterThan(0);
    expect(calls.map((c) => c.model)).toEqual(["qwen3:4b", "nemotron-3-nano:4b"]);
    // Nothing tool-shaped is forwarded raw; one system turn carries the tools.
    for (const c of calls) {
      expect(c.body.tools).toBeUndefined();
      expect(c.body.tooling).toBeUndefined();
      expect(c.body.p2p_only).toBe(true);
      expect(c.messages.filter((m) => m.role === "system")).toHaveLength(1);
      expect(c.messages[0].content).toContain("<tools>");
    }
  });

  test("402 stops at once (no fan-out across models)", async () => {
    const { up, calls } = fakeInfernet({ "qwen3:4b": [{ status: 402, error: "out of compute balance" }] });
    const res = await toolChatResponse({ upstream: up, req: toolReq(), rawModel: undefined });
    expect(res.status).toBe(402);
    expect(calls).toHaveLength(1);
  });

  test("one repair retry on an unreadable call, then success", async () => {
    const { up, calls } = fakeInfernet({
      "qwen3:4b": [{ text: "<tool_call>get_weather(Paris)</tool_call>" }, { text: '<tool_call>{"name":"get_weather","arguments":{"city":"Paris"}}</tool_call>' }],
    });
    const r = await runTools(up, toolReq({ model: "qwen3:4b" }), toolReq().tooling!, ["qwen3:4b"]);
    expect(r.reply.calls).toHaveLength(1);
    expect(r.attempts).toBe(2);
    expect(calls).toHaveLength(2);
    expect(calls[1].messages.at(-1)?.content).toMatch(/did not contain a valid tool call/);
  });

  test("a second unreadable reply is an error, never fake prose", async () => {
    const { up } = fakeInfernet({ "qwen3:4b": [{ text: "<tool_call>broken" }, { text: "<tool_call>still broken" }] });
    await expect(runTools(up, toolReq({ model: "qwen3:4b" }), toolReq().tooling!, ["qwen3:4b"])).rejects.toBeInstanceOf(ToolChatError);
  });

  test("final turn after a tool result answers in content", async () => {
    const { up, calls } = fakeInfernet({ "qwen3:4b": [{ text: "It is 18 C in Paris." }] });
    const req = parseChatRequest({
      model: "qwen3:4b",
      tools: [weather],
      messages: [
        { role: "user", content: "weather in Paris?" },
        { role: "assistant", content: null, tool_calls: [{ id: "call_a", type: "function", function: { name: "get_weather", arguments: '{"city":"Paris"}' } }] },
        { role: "tool", tool_call_id: "call_a", content: '{"temp_c":18}' },
      ],
    }) as ChatRequest;
    const res = await toolChatResponse({ upstream: up, req, rawModel: "qwen3:4b" });
    const j = (await res.json()) as ReturnType<typeof toolCompletionJson>;
    expect(j.choices[0]).toEqual({ index: 0, message: { role: "assistant", content: "It is 18 C in Paris." }, finish_reason: "stop" });
    expect(calls[0].messages.at(-1)?.content).toContain("<tool_response>");
    expect(calls).toHaveLength(1);
  });

  test("streaming: keepalive, role, tool_calls deltas, finish, usage, [DONE]", async () => {
    const { up } = fakeInfernet({ "qwen3:4b": [{ text: '<tool_call>{"name":"get_weather","arguments":{"city":"Paris"}}</tool_call>' }] });
    const res = await toolChatResponse({ upstream: up, req: toolReq({ stream: true, stream_options: { include_usage: true } }), rawModel: "auto" });
    expect(res.headers.get("content-type")).toBe("text/event-stream");
    const body = await res.text();
    expect(body.startsWith(": c0mpute tools")).toBe(true);
    const data = body.split("\n").filter((l) => l.startsWith("data: ")).map((l) => l.slice(6));
    expect(data.at(-1)).toBe("[DONE]");
    const chunks = data.slice(0, -1).map((d) => JSON.parse(d));
    expect(chunks[0].choices[0].delta.role).toBe("assistant");
    const name = chunks.find((c) => c.choices[0]?.delta?.tool_calls?.[0]?.function?.name)?.choices[0].delta.tool_calls[0];
    expect(name).toMatchObject({ index: 0, type: "function", function: { name: "get_weather" } });
    expect(chunks.find((c) => c.choices[0]?.finish_reason)?.choices[0].finish_reason).toBe("tool_calls");
    expect(chunks.at(-1).usage.total_tokens).toBeGreaterThan(0);
  });

  test("streaming error is an error frame then [DONE]", async () => {
    const { up } = fakeInfernet({}, ["qwen3:4b"]);
    const res = await toolChatResponse({ upstream: up, req: toolReq({ stream: true }), rawModel: "auto" });
    const body = await res.text();
    expect(body).toContain('"error"');
    expect(body.trim().endsWith("data: [DONE]")).toBe(true);
  });

  test("no tool-capable model online: 503", async () => {
    const { up } = fakeInfernet({}, ["gemma3:4b"]);
    await expect(toolModels(up, toolReq(), "auto")).rejects.toMatchObject({ status: 503 });
  });
});

describe("plain chat unchanged", () => {
  test("no tools: forwarded body is exactly what it was", async () => {
    let sent: Record<string, unknown> = {};
    const up: Upstream = async (_p, init) => {
      sent = JSON.parse(String(init?.body));
      return Response.json({});
    };
    const req = parseChatRequest({ model: "qwen3:4b", messages: [{ role: "user", content: "hi" }], stream: true, temperature: 0.2, max_tokens: 50 }) as ChatRequest;
    expect(req).toEqual({ model: "qwen3:4b", messages: [{ role: "user", content: "hi" }], stream: true, temperature: 0.2, max_tokens: 50 });
    await forwardChat(up, req);
    expect(sent).toEqual({
      model: "qwen3:4b",
      messages: [{ role: "system", content: C0_SYSTEM_PROMPT }, { role: "user", content: "hi" }],
      stream: true,
      temperature: 0.2,
      max_tokens: 50,
      p2p_only: true,
    });
  });
});
