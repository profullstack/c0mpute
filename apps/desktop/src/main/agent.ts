// The Agent chat's tool loop. Electron-free so it can be tested with a fake
// model and fake tools: the main process injects the c0mpute completion call,
// the MCP/skill/shell tools, the confirmation prompt and the transcript sink.

import { clip, extractToolCalls, formatToolResult, nativeToolCalls, type ToolCall, type ToolDef, toolsPrompt } from "./tool-protocol";

export interface ChatMessage {
  role: "system" | "user" | "assistant";
  content: string;
}

export interface AgentTool extends ToolDef {
  /** Who provides it: `mcp:<server>`, `skills` or `shell`. Approval is per source. */
  source: string;
  /** Ask before every run unless the source is allowed. */
  confirm: boolean;
  run(args: Record<string, unknown>, signal?: AbortSignal): Promise<string>;
}

export type Decision = "once" | "always" | "deny";

export type AgentEvent =
  | { type: "user"; text: string }
  | { type: "assistant"; text: string; model?: string }
  | { type: "tool_call"; id: string; name: string; source: string; arguments: Record<string, unknown> }
  | { type: "tool_result"; id: string; name: string; output: string; error: boolean }
  | { type: "status"; text: string }
  | { type: "error"; text: string }
  | { type: "done" }
  /** Messages waiting for the current run to finish (sent on every change). */
  | { type: "queued"; items: string[] };

export interface AgentDeps {
  complete(messages: ChatMessage[], model: string | undefined, signal?: AbortSignal): Promise<{ content: string; message?: unknown; model?: string }>;
  tools(): Promise<AgentTool[]>;
  /** Extra system prompt (the skills list). */
  systemExtra(): string;
  confirm(req: { call: ToolCall; tool: AgentTool }): Promise<Decision>;
  emit(ev: AgentEvent): void;
}

export const BASE_SYSTEM =
  "You are the c0mpute desktop agent, running on the user's own machine. Be direct and accurate. Use tools when they help; never invent tool results. When a tool fails, say so plainly.";

export class AgentSession {
  messages: ChatMessage[] = [];
  model: string | undefined;
  maxSteps: number;
  /** Sources the user allowed for this session ("always" answers). */
  allowed = new Set<string>();
  private running = false;
  private queue: string[] = [];
  private abort: AbortController | null = null;

  constructor(
    private deps: AgentDeps,
    opts: { model?: string; maxSteps?: number } = {},
  ) {
    this.model = opts.model;
    this.maxSteps = opts.maxSteps ?? 12;
  }

  get busy() {
    return this.running;
  }

  reset() {
    this.stop();
    this.messages = [];
    this.setQueue([]);
  }

  get queued(): readonly string[] {
    return this.queue;
  }

  private setQueue(items: string[]) {
    this.queue = items;
    this.deps.emit({ type: "queued", items: [...items] });
  }

  /**
   * Send a message now, or queue it while a run (model steps and tool calls)
   * is going. Queued messages go out in order as each run ends; stop() ends
   * the current run, so the next one goes at once.
   */
  async submit(text: string): Promise<void> {
    const t = text.trim();
    if (!t) return;
    if (this.running) {
      this.setQueue([...this.queue, t]);
      return;
    }
    let next: string | undefined = t;
    while (next !== undefined) {
      this.abort = new AbortController();
      await this.send(next, this.abort.signal);
      this.abort = null;
      next = this.queue[0];
      if (next !== undefined) this.setQueue(this.queue.slice(1));
    }
  }

  /** Interrupt the current run (the queue keeps going). */
  stop() {
    this.abort?.abort();
  }

  /** Take a queued message out (to edit or drop it); returns its text. */
  unqueue(index: number): string | undefined {
    const item = this.queue[index];
    if (item !== undefined) this.setQueue(this.queue.filter((_, i) => i !== index));
    return item;
  }

  private system(tools: AgentTool[]): string {
    return [BASE_SYSTEM, this.deps.systemExtra(), toolsPrompt(tools)].filter(Boolean).join("\n\n");
  }

  async send(text: string, signal?: AbortSignal): Promise<void> {
    if (this.running) throw new Error("the agent is still working on the last message");
    this.running = true;
    const { emit } = this.deps;
    try {
      this.messages.push({ role: "user", content: text });
      emit({ type: "user", text });
      const tools = await this.deps.tools();
      const byName = new Map(tools.map((t) => [t.name, t]));
      for (let step = 0; step < this.maxSteps; step++) {
        if (signal?.aborted) throw new Error("stopped");
        emit({ type: "status", text: step === 0 ? "thinking" : `thinking (step ${step + 1})` });
        const reply = await this.deps.complete([{ role: "system", content: this.system(tools) }, ...this.messages], this.model, signal);
        const parsed = extractToolCalls(reply.content ?? "");
        const calls = [...nativeToolCalls(reply.message), ...parsed.calls];
        // Keep the raw reply (with its tool_call blocks) in history so the
        // model sees what it asked for next to the results.
        this.messages.push({ role: "assistant", content: reply.content ?? "" });
        if (parsed.text) emit({ type: "assistant", text: parsed.text, model: reply.model });
        if (!calls.length) {
          emit({ type: "done" });
          return;
        }
        const results: string[] = [];
        for (const call of calls) {
          results.push(await this.runCall(call, byName, signal));
        }
        this.messages.push({ role: "user", content: results.join("\n\n") });
      }
      emit({ type: "error", text: `stopped after ${this.maxSteps} tool steps` });
      emit({ type: "done" });
    } catch (e) {
      emit({ type: "error", text: e instanceof Error ? e.message : String(e) });
      emit({ type: "done" });
    } finally {
      this.running = false;
    }
  }

  private async runCall(call: ToolCall, byName: Map<string, AgentTool>, signal?: AbortSignal): Promise<string> {
    const { emit } = this.deps;
    const tool = byName.get(call.name);
    emit({ type: "tool_call", id: call.id, name: call.name || "(unparsed)", source: tool?.source ?? "?", arguments: call.arguments });
    const fail = (msg: string) => {
      emit({ type: "tool_result", id: call.id, name: call.name, output: msg, error: true });
      return formatToolResult(call, msg, true);
    };
    if (call.error) return fail(call.error);
    if (!tool) return fail(`no tool named ${JSON.stringify(call.name)}; available: ${[...byName.keys()].join(", ") || "none"}`);
    if (tool.confirm && !this.allowed.has(tool.source)) {
      const d = await this.deps.confirm({ call, tool });
      if (d === "deny") return fail("the user declined to run this tool call");
      // The shell is confirmed command by command, whatever the answer.
      if (d === "always" && tool.source !== "shell") this.allowed.add(tool.source);
    }
    try {
      const out = await tool.run(call.arguments, signal);
      emit({ type: "tool_result", id: call.id, name: call.name, output: clip(out, 4000), error: false });
      return formatToolResult(call, out, false);
    } catch (e) {
      return fail(e instanceof Error ? e.message : String(e));
    }
  }
}
