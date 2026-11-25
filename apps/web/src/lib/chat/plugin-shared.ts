// Client-safe half of chat plugins: the `/plugin` command and the stream event
// shape. No Node imports: chat-app.tsx bundles this for the browser.

export type PluginCommand =
  | { kind: "add"; target: string }
  | { kind: "ls" }
  | { kind: "rm"; slug: string }
  | { kind: "on" | "off"; slug: string }
  | { kind: "connect"; slug: string }
  | { kind: "paper" | "live"; slug: string }
  | { kind: "rename"; slug: string; title: string }
  /** The key itself is asked for in a prompt, never typed into the chat (the transcript is saved). */
  | { kind: "key"; slug: string; clear: boolean }
  | { kind: "help"; error?: string };

const SLUGISH = /^[a-z0-9][a-z0-9-]{0,31}$/;

/**
 * `/plugin add <url|catalog slug>`, `/plugin ls`, `/plugin rm <slug>`,
 * `/plugin on|off <slug>`, `/plugin connect <slug>`; `/plugins` = ls.
 */
export function parsePluginCommand(text: string): PluginCommand | null {
  const m = /^\/plugins?(?:\s+([\s\S]*))?$/i.exec(String(text ?? "").trim());
  if (!m) return null;
  const words = (m[1] ?? "").trim().split(/\s+/).filter(Boolean);
  const [verbRaw, arg] = words;
  const verb = (verbRaw ?? "").toLowerCase();
  if (!verb) return /^\/plugins\b/i.test(text.trim()) ? { kind: "ls" } : { kind: "help" };
  if (verb === "ls" || verb === "list") return { kind: "ls" };
  if (verb === "add" || verb === "install") {
    if (!arg) return { kind: "help", error: "usage: /plugin add <https://… MCP server URL | catalog name>" };
    return { kind: "add", target: arg };
  }
  // `/plugin https://…` is add.
  if (/^https?:\/\//i.test(verbRaw)) return { kind: "add", target: verbRaw };
  const slug = (arg ?? "").toLowerCase();
  const need = (k: "rm" | "on" | "off" | "connect" | "paper" | "live") =>
    SLUGISH.test(slug) ? ({ kind: k, slug } as PluginCommand) : ({ kind: "help", error: `usage: /plugin ${k} <name>` } as PluginCommand);
  if (verb === "rm" || verb === "remove" || verb === "delete") return need("rm");
  if (verb === "on" || verb === "enable") return need("on");
  if (verb === "off" || verb === "disable") return need("off");
  if (verb === "connect" || verb === "signin" || verb === "login") return need("connect");
  if (verb === "paper" || verb === "live") return need(verb);
  if (verb === "rename" || verb === "title") {
    const title = words.slice(2).join(" ").trim();
    if (!SLUGISH.test(slug)) return { kind: "help", error: "usage: /plugin rename <name> <new title>" };
    return { kind: "rename", slug, title };
  }
  if (verb === "key" || verb === "apikey" || verb === "api-key") {
    if (!SLUGISH.test(slug)) return { kind: "help", error: "usage: /plugin key <name> (asks for the key) | /plugin key <name> clear" };
    // A key pasted after the name is refused: it would be saved in the chat.
    const rest = words.slice(2);
    if (rest.length && !(rest.length === 1 && /^(clear|rm|remove|none)$/i.test(rest[0])))
      return { kind: "help", error: "do not type the key into the chat (it is saved with the conversation): run /plugin key <name> and paste it into the box that opens" };
    return { kind: "key", slug, clear: rest.length === 1 };
  }
  return { kind: "help", error: `unknown /plugin command "${verbRaw}"` };
}

export const PLUGIN_HELP = [
  "/plugin add <url | name>   add a remote MCP server (or one from the catalog: /plugin ls)",
  "/plugin ls                 your plugins and the catalog",
  "/plugin on|off <name>      use it in chat, or not",
  "/plugin connect <name>     sign in again",
  "/plugin key <name>         set its API key (servers that take a key, not a sign-in); `clear` removes it",
  "/plugin rename <name> <t>  change the label it shows",
  "/plugin paper|live <name>  brokerage plugins: paper trading (the default) or your live account",
  "/plugin rm <name>          remove it",
].join("\n");

/** One event of a plugin-assisted chat turn, as the SSE stream carries it. */
export type PluginEvent =
  | { type: "tool_call"; id: string; plugin: string; tool: string; arguments: Record<string, unknown> }
  | { type: "tool_result"; id: string; plugin: string; tool: string; ok: boolean; text: string }
  | {
      type: "approval_needed";
      id: string;
      plugin: string;
      tool: string;
      arguments: Record<string, unknown>;
      approval: string;
      /** Money plugins: the exact action in words (amount, currency, from, to, memo; or the order and paper/live). */
      summary?: import("./plugin-policy").ActionSummary;
    }
  | { type: "plugin_error"; plugin: string; message: string };

/** SSE `data:` objects for plugin events carry this `object` so OpenAI clients skip them. */
export const PLUGIN_EVENT_OBJECT = "c0mpute.plugin_event";
