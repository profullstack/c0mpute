// ~/mcps: the user's MCP servers, one or many per `*.json` file, in the shape
// Claude Code (.mcp.json) and Claude Desktop use:
//
//   { "_note": "...", "mcpServers": { "<name>": { "command", "args", "env" }
//                                     | { "type": "http", "url", "headers" } } }
//
// Unknown top-level keys (`_note`) and unknown per-server keys are kept when a
// file is written back. `${VAR}` placeholders stay placeholders on disk; they
// are expanded only in memory, at connect time.

import { existsSync, mkdirSync, readdirSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { basename, join } from "node:path";

export type McpTransport = "stdio" | "http" | "sse";

export interface McpServer {
  name: string;
  /** The file it was read from (absolute). */
  file: string;
  transport: McpTransport;
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  cwd?: string;
  url?: string;
  headers?: Record<string, string>;
  disabled: boolean;
  /** The file's `_note`, when it has one (shown in the UI). */
  note?: string;
}

export interface McpLoad {
  servers: McpServer[];
  errors: { file: string; error: string }[];
}

export const NAME_RE = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/;

function strMap(v: unknown): Record<string, string> | undefined {
  if (!v || typeof v !== "object" || Array.isArray(v)) return undefined;
  const out: Record<string, string> = {};
  for (const [k, x] of Object.entries(v as Record<string, unknown>)) {
    if (typeof x === "string") out[k] = x;
    else if (typeof x === "number" || typeof x === "boolean") out[k] = String(x);
  }
  return out;
}

export function entryToServer(name: string, raw: unknown, file: string, note?: string): McpServer {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) throw new Error(`${name}: not an object`);
  const e = raw as Record<string, unknown>;
  const type = typeof e.type === "string" ? e.type.toLowerCase() : undefined;
  const disabled = e.disabled === true;
  if (typeof e.url === "string" || type === "http" || type === "sse" || type === "streamable-http") {
    if (typeof e.url !== "string" || !/^https?:\/\//.test(e.url)) throw new Error(`${name}: http server needs an http(s) url`);
    return { name, file, transport: type === "sse" ? "sse" : "http", url: e.url, headers: strMap(e.headers), disabled, note };
  }
  if (typeof e.command !== "string" || !e.command.trim()) throw new Error(`${name}: needs "command" (stdio) or "url" (http)`);
  const args = Array.isArray(e.args) ? e.args.map((a) => String(a)) : [];
  return {
    name,
    file,
    transport: "stdio",
    command: e.command,
    args,
    env: strMap(e.env),
    cwd: typeof e.cwd === "string" ? e.cwd : undefined,
    disabled,
    note,
  };
}

/** Parse one file's text. Bad entries become errors; the rest still load. */
export function parseMcpFile(text: string, file: string): McpLoad {
  const out: McpLoad = { servers: [], errors: [] };
  let doc: unknown;
  try {
    doc = JSON.parse(text);
  } catch (e) {
    out.errors.push({ file, error: `not JSON: ${e instanceof Error ? e.message : e}` });
    return out;
  }
  if (!doc || typeof doc !== "object" || Array.isArray(doc)) {
    out.errors.push({ file, error: "not a JSON object" });
    return out;
  }
  const d = doc as Record<string, unknown>;
  const note = typeof d._note === "string" ? d._note : undefined;
  const servers = d.mcpServers;
  if (!servers || typeof servers !== "object" || Array.isArray(servers)) {
    out.errors.push({ file, error: 'no "mcpServers" object' });
    return out;
  }
  for (const [name, raw] of Object.entries(servers as Record<string, unknown>)) {
    try {
      if (!NAME_RE.test(name)) throw new Error(`bad server name ${JSON.stringify(name)}`);
      out.servers.push(entryToServer(name, raw, file, note));
    } catch (e) {
      out.errors.push({ file, error: e instanceof Error ? e.message : String(e) });
    }
  }
  return out;
}

export function defaultMcpDir(): string {
  return process.env.C0MPUTE_MCPS_DIR || join(homedir(), "mcps");
}

/** Load every *.json in the dir (created on first run). Later duplicates lose. */
export function loadMcpDir(dir = defaultMcpDir()): McpLoad {
  if (!existsSync(dir)) mkdirSync(dir, { recursive: true });
  const out: McpLoad = { servers: [], errors: [] };
  const seen = new Set<string>();
  for (const f of readdirSync(dir).filter((n) => n.endsWith(".json")).sort()) {
    const file = join(dir, f);
    let text: string;
    try {
      text = readFileSync(file, "utf8");
    } catch (e) {
      out.errors.push({ file, error: String(e) });
      continue;
    }
    const r = parseMcpFile(text, file);
    out.errors.push(...r.errors);
    for (const s of r.servers) {
      if (seen.has(s.name)) {
        out.errors.push({ file, error: `${s.name}: already defined in another file; this one is ignored` });
        continue;
      }
      seen.add(s.name);
      out.servers.push(s);
    }
  }
  return out;
}

/** The JSON entry for a server (what goes under mcpServers.<name>). */
export function serverToEntry(s: McpServer, keep: Record<string, unknown> = {}): Record<string, unknown> {
  const e: Record<string, unknown> = { ...keep };
  for (const k of ["type", "url", "headers", "command", "args", "env", "cwd", "disabled"]) delete e[k];
  if (s.transport === "stdio") {
    e.command = s.command;
    if (s.args?.length) e.args = s.args;
    if (s.env && Object.keys(s.env).length) e.env = s.env;
    if (s.cwd) e.cwd = s.cwd;
  } else {
    e.type = s.transport;
    e.url = s.url;
    if (s.headers && Object.keys(s.headers).length) e.headers = s.headers;
  }
  if (s.disabled) e.disabled = true;
  return e;
}

/** File text with `server` added or replaced; other servers and keys kept. */
export function upsertServerText(existing: string | null, server: McpServer, previousName?: string): string {
  let doc: Record<string, unknown> = {};
  if (existing) {
    const parsed = JSON.parse(existing);
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) doc = parsed;
  }
  const servers = (doc.mcpServers && typeof doc.mcpServers === "object" ? doc.mcpServers : {}) as Record<string, unknown>;
  const keep = (servers[previousName ?? server.name] ?? {}) as Record<string, unknown>;
  if (previousName && previousName !== server.name) delete servers[previousName];
  servers[server.name] = serverToEntry(server, typeof keep === "object" ? keep : {});
  doc.mcpServers = servers;
  return `${JSON.stringify(doc, null, 2)}\n`;
}

/** File text without `name`, or null when no server is left (delete the file). */
export function removeServerText(existing: string, name: string): string | null {
  const doc = JSON.parse(existing) as Record<string, unknown>;
  const servers = (doc.mcpServers ?? {}) as Record<string, unknown>;
  delete servers[name];
  if (Object.keys(servers).length === 0) return null;
  doc.mcpServers = servers;
  return `${JSON.stringify(doc, null, 2)}\n`;
}

function atomicWrite(file: string, text: string) {
  const tmp = `${file}.tmp-${process.pid}`;
  writeFileSync(tmp, text, { mode: 0o600 });
  renameSync(tmp, file);
}

export function safeFileName(name: string): string {
  return `${name.replace(/[^A-Za-z0-9_.-]/g, "-").replace(/^\.+/, "") || "server"}.json`;
}

/**
 * Save a server: into the file it came from, or `<dir>/<name>.json` for a new
 * one. Masked values the UI sent back unchanged are restored from disk first.
 */
export function saveServer(dir: string, server: McpServer, previous?: McpServer): string {
  if (!NAME_RE.test(server.name)) throw new Error("server name: letters, digits, '.', '_' or '-', up to 64");
  const file = previous?.file ?? join(dir, safeFileName(server.name));
  if (!previous && existsSync(file)) {
    const other = parseMcpFile(readFileSync(file, "utf8"), file).servers;
    if (other.some((s) => s.name === server.name)) throw new Error(`${server.name} already exists`);
  }
  const merged = previous ? unmask(server, previous) : server;
  entryToServer(merged.name, serverToEntry(merged), file); // validates
  const existing = existsSync(file) ? readFileSync(file, "utf8") : null;
  atomicWrite(file, upsertServerText(existing, merged, previous?.name));
  return file;
}

export function deleteServer(server: McpServer): void {
  const text = readFileSync(server.file, "utf8");
  const next = removeServerText(text, server.name);
  if (next === null) unlinkSync(server.file);
  else atomicWrite(server.file, next);
}

// ── secrets ───────────────────────────────────────────────────────────────

const SENSITIVE = /KEY|TOKEN|SECRET|PASSWORD|PASSWD|AUTH|COOKIE|CREDENTIAL/i;
const PLACEHOLDER = /\$\{[A-Za-z_][A-Za-z0-9_]*\}/g;
export const MASK = "••••••••";

export function isSensitiveKey(k: string): boolean {
  return SENSITIVE.test(k);
}

/** A value with nothing secret in it: only `${VAR}` placeholders (and an auth scheme word). */
export function isPlaceholderOnly(v: string): boolean {
  const rest = v.replace(PLACEHOLDER, "").replace(/^\s*(Bearer|Basic|Token)\s*/i, "").trim();
  return rest === "";
}

export function maskValue(key: string, value: string): string {
  if (!isSensitiveKey(key) || isPlaceholderOnly(value) || value === "") return value;
  const scheme = /^(Bearer|Basic|Token)\s+/i.exec(value)?.[0] ?? "";
  return `${scheme}${MASK}`;
}

function maskMap(m?: Record<string, string>) {
  if (!m) return m;
  return Object.fromEntries(Object.entries(m).map(([k, v]) => [k, maskValue(k, v)]));
}

/** A copy safe to show in the UI or a log. */
export function maskServer(s: McpServer): McpServer {
  return { ...s, env: maskMap(s.env), headers: maskMap(s.headers) };
}

/** Put real values back where the UI returned a mask unchanged. */
export function unmask(edited: McpServer, original: McpServer): McpServer {
  const fix = (m?: Record<string, string>, o?: Record<string, string>) => {
    if (!m) return m;
    const out: Record<string, string> = {};
    for (const [k, v] of Object.entries(m)) {
      out[k] = v.includes(MASK) && o?.[k] !== undefined ? o[k] : v;
    }
    return out;
  };
  return { ...edited, env: fix(edited.env, original.env), headers: fix(edited.headers, original.headers) };
}

/** Names of the `${VAR}`s a server needs. */
export function placeholders(s: McpServer): string[] {
  const vals = [...Object.values(s.env ?? {}), ...Object.values(s.headers ?? {}), s.url ?? "", ...(s.args ?? [])];
  const names = new Set<string>();
  for (const v of vals) for (const m of v.matchAll(PLACEHOLDER)) names.add(m[0].slice(2, -1));
  return [...names];
}

export function expand(value: string, lookup: (name: string) => string | undefined): { value: string; missing: string[] } {
  const missing: string[] = [];
  const out = value.replace(PLACEHOLDER, (m) => {
    const name = m.slice(2, -1);
    const v = lookup(name);
    if (v === undefined) {
      missing.push(name);
      return "";
    }
    return v;
  });
  return { value: out, missing };
}

/** The server with placeholders filled in, for connecting only (never saved). */
export function expandServer(s: McpServer, lookup: (name: string) => string | undefined): { server: McpServer; missing: string[] } {
  const missing = new Set<string>();
  const ex = (v: string) => {
    const r = expand(v, lookup);
    for (const m of r.missing) missing.add(m);
    return r.value;
  };
  const map = (m?: Record<string, string>) => (m ? Object.fromEntries(Object.entries(m).map(([k, v]) => [k, ex(v)])) : m);
  return {
    server: {
      ...s,
      url: s.url ? ex(s.url) : s.url,
      args: s.args?.map(ex),
      env: map(s.env),
      headers: map(s.headers),
    },
    missing: [...missing],
  };
}

// ── import from Claude Code / Claude Desktop ──────────────────────────────

export function claudeConfigPaths(home = homedir()): string[] {
  const paths = [join(home, ".claude.json"), join(home, ".mcp.json"), join(home, ".config", "Claude", "claude_desktop_config.json")];
  if (process.platform === "darwin") paths.push(join(home, "Library", "Application Support", "Claude", "claude_desktop_config.json"));
  if (process.platform === "win32" && process.env.APPDATA) paths.push(join(process.env.APPDATA, "Claude", "claude_desktop_config.json"));
  return paths;
}

/**
 * Servers in Claude Code's ~/.claude.json (top-level and per-project
 * mcpServers) and Claude Desktop's config. Each comes back with `file` set to
 * where it would be saved in `dir`.
 */
export function readClaudeServers(paths: string[], dir: string): McpServer[] {
  const out = new Map<string, McpServer>();
  const take = (servers: unknown, source: string) => {
    if (!servers || typeof servers !== "object") return;
    for (const [name, raw] of Object.entries(servers as Record<string, unknown>)) {
      if (!NAME_RE.test(name) || out.has(name)) continue;
      try {
        const s = entryToServer(name, raw, join(dir, safeFileName(name)));
        s.note = `imported from ${basename(source)}`;
        out.set(name, s);
      } catch {
        /* skip entries we cannot run */
      }
    }
  };
  for (const p of paths) {
    if (!existsSync(p)) continue;
    let doc: Record<string, unknown>;
    try {
      doc = JSON.parse(readFileSync(p, "utf8"));
    } catch {
      continue;
    }
    take(doc.mcpServers, p);
    const projects = doc.projects as Record<string, { mcpServers?: unknown }> | undefined;
    if (projects && typeof projects === "object") for (const pr of Object.values(projects)) take(pr?.mcpServers, p);
  }
  return [...out.values()];
}

/** Copy imported servers into `dir`, skipping names that already exist. */
export function importServers(dir: string, servers: McpServer[], existing: McpServer[]): { added: string[]; skipped: string[] } {
  mkdirSync(dir, { recursive: true });
  const have = new Set(existing.map((s) => s.name));
  const added: string[] = [];
  const skipped: string[] = [];
  for (const s of servers) {
    if (have.has(s.name)) {
      skipped.push(s.name);
      continue;
    }
    const file = join(dir, safeFileName(s.name));
    if (existsSync(file)) {
      skipped.push(s.name);
      continue;
    }
    const doc = { _note: s.note ?? "imported", mcpServers: { [s.name]: serverToEntry(s) } };
    atomicWrite(file, `${JSON.stringify(doc, null, 2)}\n`);
    added.push(s.name);
  }
  return { added, skipped };
}
