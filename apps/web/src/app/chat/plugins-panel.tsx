"use client";

// Chat plugins in the web chat: the Plugins panel (catalog one-click Add with a
// popup sign-in, custom URL, on/off, remove) and the cards a plugin-assisted
// turn shows (tool calls, their results, approvals for destructive tools).
// Everything a plugin returns is untrusted: it is rendered as text only.

import { useCallback, useEffect, useState } from "react";

export type PluginRow = {
  slug: string;
  url: string;
  name: string;
  title: string | null;
  enabled: boolean;
  auth: "oauth" | "none";
  connected: boolean;
  status: "on" | "off" | "needs_sign_in";
  tools: { name: string; description: string; destructive: boolean }[];
};
export type CatalogRow = { slug: string; name: string; title: string; description: string; url: string; added: boolean };

/** One tool step of an assistant turn. */
export type ToolView = {
  id: string;
  plugin: string;
  tool: string;
  arguments: Record<string, unknown>;
  status: "running" | "ok" | "error" | "approval" | "approved" | "denied";
  text?: string;
  approval?: string;
};

/**
 * Open a plugin's sign-in in a popup; resolves true once the callback page
 * reports back. Where window.open is refused (a popup blocker, or the desktop
 * app, which opens its own sign-in window), it polls the plugin until it is
 * connected instead (up to 5 minutes).
 */
export function signInPopup(connectUrl: string, slug: string): Promise<boolean> {
  return new Promise((resolve) => {
    const w = window.open(connectUrl, "c0-plugin-signin", "popup,width=520,height=720");
    let done = false;
    const started = Date.now();
    const finish = (ok: boolean) => {
      if (done) return;
      done = true;
      window.removeEventListener("message", onMsg);
      clearInterval(t);
      resolve(ok);
    };
    const onMsg = (e: MessageEvent) => {
      if (e.origin !== window.location.origin) return;
      if (e.data && e.data.type === "c0-plugin-connected" && (!e.data.slug || e.data.slug === slug)) finish(true);
    };
    window.addEventListener("message", onMsg);
    let n = 0;
    const t = setInterval(async () => {
      n++;
      if (w && w.closed) {
        // Closed: one last look, the callback may have finished just now.
        const ok = await connected(slug);
        return finish(ok);
      }
      if (!w && n % 4 === 0) {
        if (await connected(slug)) return finish(true);
        if (Date.now() - started > 5 * 60_000) finish(false);
      }
    }, 500);
  });
}

async function connected(slug: string): Promise<boolean> {
  try {
    const r = await fetch(`/api/v1/chat/plugins/${encodeURIComponent(slug)}`);
    if (!r.ok) return false;
    const j = await r.json();
    return !!j?.plugin?.connected;
  } catch {
    return false;
  }
}

async function api<T>(path: string, init: RequestInit = {}): Promise<T> {
  const r = await fetch(path, { ...init, headers: { "content-type": "application/json", ...(init.headers ?? {}) } });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(j?.error?.message ?? `HTTP ${r.status}`);
  return j as T;
}

export const pluginsApi = {
  list: () => api<{ plugins: PluginRow[]; catalog: CatalogRow[] }>("/api/v1/chat/plugins"),
  add: (target: string) =>
    api<{ plugin: PluginRow; connect_url: string | null }>("/api/v1/chat/plugins", {
      method: "POST",
      body: JSON.stringify(/^https?:\/\//i.test(target) ? { url: target } : { slug: target }),
    }),
  remove: (slug: string) => api<{ ok: boolean }>(`/api/v1/chat/plugins/${encodeURIComponent(slug)}`, { method: "DELETE" }),
  toggle: (slug: string, enabled?: boolean) =>
    api<{ plugin: PluginRow }>(`/api/v1/chat/plugins/${encodeURIComponent(slug)}/toggle`, {
      method: "POST",
      body: JSON.stringify(enabled === undefined ? {} : { enabled }),
    }),
};

export function PluginsPanel() {
  const [plugins, setPlugins] = useState<PluginRow[]>([]);
  const [catalog, setCatalog] = useState<CatalogRow[]>([]);
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const j = await pluginsApi.list();
      setPlugins(j.plugins);
      setCatalog(j.catalog);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  }, []);
  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function add(target: string) {
    setErr(null);
    setNote(null);
    setBusy(target);
    try {
      const r = await pluginsApi.add(target);
      if (r.connect_url) {
        setNote(`signing in to ${r.plugin.name}…`);
        const ok = await signInPopup(r.connect_url, r.plugin.slug);
        setNote(ok ? `${r.plugin.name} connected` : `${r.plugin.name} added; finish signing in with "sign in"`);
      } else {
        setNote(`${r.plugin.name} added (${r.plugin.tools.length} tools)`);
      }
      setUrl("");
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
      void refresh();
    }
  }

  async function act(slug: string, fn: () => Promise<unknown>) {
    setErr(null);
    setBusy(slug);
    try {
      await fn();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
      void refresh();
    }
  }

  const addable = catalog.filter((c) => !c.added);

  return (
    <section className="border border-[var(--color-rule)] bg-[var(--color-panel)] p-4 space-y-6 text-sm min-w-0">
      <header className="space-y-1">
        <h2 className="accent text-base">chat plugins</h2>
        <p className="comment">
          // a plugin is a remote MCP server. Add one and the model can use its tools in your chats. Sign-in is automatic. Or type /plugin add &lt;url&gt; in the chat.
        </p>
      </header>
      {err && <p className="text-[var(--color-warn)] break-words">{err}</p>}
      {note && <p className="text-[var(--color-dim)]">{note}</p>}

      <div className="space-y-2">
        <h3 className="text-xs text-[var(--color-dim)]">yours</h3>
        {plugins.length === 0 && <p className="text-[var(--color-dim)]">none yet</p>}
        <ul className="space-y-2">
          {plugins.map((p) => (
            <li key={p.slug} className="border border-[var(--color-rule)] p-2 space-y-1">
              <div className="flex flex-wrap items-center gap-2">
                <span className="accent">{p.slug}</span>
                <span className="text-[var(--color-dim)] truncate">{p.title || p.name}</span>
                <span className="ml-auto flex items-center gap-2 text-xs">
                  {p.status === "needs_sign_in" ? (
                    <button
                      disabled={busy === p.slug}
                      className="border border-[var(--color-accent)] px-2 accent"
                      onClick={() => act(p.slug, async () => signInPopup(`/api/v1/chat/plugins/${encodeURIComponent(p.slug)}/connect`, p.slug))}
                    >
                      sign in
                    </button>
                  ) : (
                    <label className="flex items-center gap-1 cursor-pointer">
                      <input type="checkbox" checked={p.enabled} disabled={busy === p.slug} onChange={(e) => act(p.slug, () => pluginsApi.toggle(p.slug, e.target.checked))} />
                      {p.enabled ? "on" : "off"}
                    </label>
                  )}
                  <button
                    disabled={busy === p.slug}
                    className="text-[var(--color-dim)] hover:text-[var(--color-warn)]"
                    onClick={() => window.confirm(`Remove ${p.slug}? Its sign-in is forgotten.`) && act(p.slug, () => pluginsApi.remove(p.slug))}
                  >
                    remove
                  </button>
                </span>
              </div>
              <p className="text-xs text-[var(--color-dim)] break-all">{p.url}</p>
              {p.tools.length > 0 && (
                <details className="text-xs">
                  <summary className="cursor-pointer text-[var(--color-dim)]">{p.tools.length} tools</summary>
                  <ul className="mt-1 space-y-0.5">
                    {p.tools.map((t) => (
                      <li key={t.name}>
                        <code>{t.name}</code>
                        {t.destructive && <span className="text-[var(--color-warn)]"> · asks first</span>}
                        {t.description && <span className="text-[var(--color-dim)]"> · {t.description.slice(0, 140)}</span>}
                      </li>
                    ))}
                  </ul>
                </details>
              )}
            </li>
          ))}
        </ul>
      </div>

      {addable.length > 0 && (
        <div className="space-y-2">
          <h3 className="text-xs text-[var(--color-dim)]">catalog</h3>
          <ul className="grid gap-2 sm:grid-cols-2">
            {addable.map((c) => (
              <li key={c.slug} className="border border-[var(--color-rule)] p-2 flex flex-col gap-1">
                <span className="accent">{c.title || c.name}</span>
                <span className="text-xs text-[var(--color-dim)] flex-1">{c.description}</span>
                <button disabled={!!busy} className="self-start border border-[var(--color-accent)] px-3 py-1 accent hover:bg-[var(--color-accent-dim)]" onClick={() => add(c.slug)}>
                  {busy === c.slug ? "adding…" : "add"}
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}

      <form
        className="space-y-2"
        onSubmit={(e) => {
          e.preventDefault();
          if (url.trim()) void add(url.trim());
        }}
      >
        <h3 className="text-xs text-[var(--color-dim)]">any MCP server</h3>
        <div className="flex gap-2">
          <input
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            placeholder="https://example.com/mcp"
            inputMode="url"
            className="flex-1 min-w-0 bg-[var(--color-card)] border border-[var(--color-rule)] px-2 py-1"
          />
          <button disabled={!!busy || !url.trim()} className="border border-[var(--color-accent)] px-3 accent hover:bg-[var(--color-accent-dim)]">
            add
          </button>
        </div>
      </form>
    </section>
  );
}

const short = (v: unknown, n = 300) => {
  const s = typeof v === "string" ? v : JSON.stringify(v);
  return s.length > n ? `${s.slice(0, n)}…` : s;
};

/** A tool step in a chat turn. The result is untrusted text: rendered as text, never HTML. */
export function ToolCard({ t, onApprove, onDeny }: { t: ToolView; onApprove?: () => void; onDeny?: () => void }) {
  const label =
    t.status === "running"
      ? "running…"
      : t.status === "ok"
        ? "done"
        : t.status === "error"
          ? "failed"
          : t.status === "approval"
            ? "wants to run this"
            : t.status === "approved"
              ? "approved"
              : "denied";
  return (
    <div className={`mt-2 border px-2 py-1 text-xs max-w-[640px] ${t.status === "approval" ? "border-[var(--color-warn)]" : "border-[var(--color-rule)]"}`}>
      <div className="flex flex-wrap gap-2">
        <span className="accent">
          {t.plugin} · {t.tool}
        </span>
        <span className={t.status === "error" || t.status === "denied" ? "text-[var(--color-warn)]" : "text-[var(--color-dim)]"}>{label}</span>
      </div>
      {Object.keys(t.arguments ?? {}).length > 0 && <p className="text-[var(--color-dim)] break-all">{short(t.arguments)}</p>}
      {t.status === "approval" && (
        <div className="flex gap-2 mt-1">
          <button className="border border-[var(--color-accent)] px-2 accent" onClick={onApprove}>
            approve
          </button>
          <button className="border border-[var(--color-rule)] px-2 text-[var(--color-dim)]" onClick={onDeny}>
            deny
          </button>
        </div>
      )}
      {t.text && (
        <details>
          <summary className="cursor-pointer text-[var(--color-dim)]">result</summary>
          <pre className="whitespace-pre-wrap break-words max-h-64 overflow-y-auto">{t.text}</pre>
        </details>
      )}
    </div>
  );
}
