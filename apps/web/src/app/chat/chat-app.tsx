"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { CodeBlock } from "@/components/CodeBlock";
import { parseImageCommand, textHistory } from "@/lib/chat/images";
import { audioExtension, parseMusicCommand, type MusicCommand } from "@/lib/chat/music";

// An image turn keeps its picture only for this session (a blob: URL); what is
// saved to localStorage is the text placeholder, and the model never sees it.
// Music turns work the same way (`audio` is a session blob: URL, `audioType` its MIME type).
type Msg = {
  role: "user" | "assistant" | "system";
  content: string;
  imageTurn?: boolean;
  image?: string;
  audio?: string;
  audioType?: string;
};
type Convo = { id: string; title: string; model: string; messages: Msg[]; updated: number };
type Balance = { spendable_usd: string; earned_spendable_usd: string; prepaid_available_usd: string };
type Me = { id: string; did: string; name: string | null; balance?: Balance | null };
type Key = { id: string; label: string; prefix: string; created_at: string; last_used_at: string | null };

const STORE = "c0_chats_v1";

/**
 * What this person can spend on inference: money they topped up on Infernet
 * plus what their own nodes earned. Chat still works at zero - the network
 * serves the free, rate-limited tier - so this is information, not a gate.
 */
function BalanceLine({ balance }: { balance: Balance | null }) {
  if (!balance) {
    return (
      <p className="text-[var(--color-dim)]">
        no balance ·{" "}
        <a className="hover:text-[var(--color-fg)] underline" href="https://infernetprotocol.com/dashboard/billing" target="_blank" rel="noreferrer">
          top up
        </a>
      </p>
    );
  }
  const spendable = Number(balance.spendable_usd ?? 0);
  const earned = Number(balance.earned_spendable_usd ?? 0);
  return (
    <p title={`prepaid $${balance.prepaid_available_usd} · earned $${balance.earned_spendable_usd}`}>
      balance{" "}
      <span className={spendable > 0 ? "accent" : "text-[var(--color-warn)]"}>
        ${spendable < 0.01 && spendable > 0 ? spendable.toFixed(6) : spendable.toFixed(2)}
      </span>
      {earned > 0 ? <span className="text-[var(--color-dim)]"> (incl. earned)</span> : null} ·{" "}
      <a className="hover:text-[var(--color-fg)] underline" href="https://infernetprotocol.com/dashboard/billing" target="_blank" rel="noreferrer">
        top up
      </a>
    </p>
  );
}

function load(): Convo[] {
  try {
    const v = JSON.parse(localStorage.getItem(STORE) ?? "[]");
    return Array.isArray(v) ? v : [];
  } catch {
    return [];
  }
}
function save(c: Convo[]) {
  try {
    const slim = c.slice(0, 100).map((cv) => ({
      ...cv,
      messages: cv.messages.map(({ image: _image, audio: _audio, audioType: _audioType, ...m }) => m),
    }));
    localStorage.setItem(STORE, JSON.stringify(slim));
  } catch {}
}
const newConvo = (model: string): Convo => ({
  id: Math.random().toString(36).slice(2),
  title: "new chat",
  model,
  messages: [],
  updated: Date.now(),
});

/** Read an OpenAI-style SSE body, calling onDelta with each content piece. */
async function readStream(res: Response, onDelta: (t: string) => void) {
  const reader = res.body!.getReader();
  const dec = new TextDecoder();
  let buf = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    let nl;
    while ((nl = buf.indexOf("\n")) !== -1) {
      const line = buf.slice(0, nl).trim();
      buf = buf.slice(nl + 1);
      if (!line.startsWith("data:")) continue;
      const data = line.slice(5).trim();
      if (data === "[DONE]") return;
      let j;
      try {
        j = JSON.parse(data);
      } catch {
        continue;
      }
      if (j.error) throw new Error(j.error.message ?? "stream error");
      const t = j.choices?.[0]?.delta?.content;
      if (t) onDelta(t);
    }
  }
}

export function ChatApp({ error }: { error: string | null }) {
  const [me, setMe] = useState<Me | null | undefined>(undefined);
  const [models, setModels] = useState<string[]>([]);
  const [convos, setConvos] = useState<Convo[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [tab, setTab] = useState<"chat" | "connect">("chat");
  const abort = useRef<AbortController | null>(null);
  const bottom = useRef<HTMLDivElement>(null);

  useEffect(() => {
    fetch("/api/v1/me")
      .then((r) => (r.ok ? r.json() : null))
      .then(setMe)
      .catch(() => setMe(null));
    fetch("/api/v1/models")
      .then((r) => (r.ok ? r.json() : { data: [] }))
      .then((j) => setModels((j.data ?? []).map((m: { id: string }) => m.id)))
      .catch(() => {});
    const c = load();
    setConvos(c);
    setActiveId(c[0]?.id ?? null);
  }, []);

  const active = convos.find((c) => c.id === activeId) ?? null;

  const update = useCallback((id: string, fn: (c: Convo) => Convo) => {
    setConvos((all) => {
      const next = all.map((c) => (c.id === id ? fn(c) : c)).sort((a, b) => b.updated - a.updated);
      save(next);
      return next;
    });
  }, []);

  useEffect(() => {
    bottom.current?.scrollIntoView({ block: "end" });
  }, [active?.messages]);

  async function drawImage(convo: Convo, text: string, prompt: string) {
    const id = convo.id;
    const title = convo.messages.length === 0 ? text.slice(0, 60) : convo.title;
    setInput("");
    setFailure(null);
    setBusy(true);
    const started = Date.now();
    const setLast = (patch: Partial<Msg>) =>
      update(id, (c) => {
        const msgs = c.messages.slice();
        msgs[msgs.length - 1] = { ...msgs[msgs.length - 1], ...patch };
        return { ...c, messages: msgs, updated: Date.now() };
      });
    setConvos((all) => {
      const exists = all.some((c) => c.id === id);
      const base = exists ? all : [convo, ...all];
      const next = base.map((c) =>
        c.id === id
          ? {
              ...c,
              title,
              messages: [
                ...c.messages,
                { role: "user" as const, content: text, imageTurn: true },
                { role: "assistant" as const, content: "// drawing on a c0mpute CPU worker, about 2 minutes…", imageTurn: true },
              ],
              updated: Date.now(),
            }
          : c,
      );
      save(next);
      return next;
    });
    const ctrl = new AbortController();
    abort.current = ctrl;
    try {
      const res = await fetch("/api/v1/images", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ prompt }),
        signal: ctrl.signal,
      });
      const start = await res.json().catch(() => ({}));
      if (res.status !== 202 || !start.poll) throw new Error(start.error?.message ?? `HTTP ${res.status}`);
      const deadline = Date.now() + 15 * 60_000;
      let done: Response | null = null;
      while (!done) {
        if (Date.now() > deadline) throw new Error("image timed out");
        await new Promise((r) => setTimeout(r, 4000));
        if (ctrl.signal.aborted) throw new DOMException("aborted", "AbortError");
        const poll = await fetch(start.poll, { cache: "no-store", signal: ctrl.signal });
        if (poll.status === 202) {
          setLast({ content: `// drawing on a c0mpute CPU worker… ${Math.round((Date.now() - started) / 1000)}s` });
          continue;
        }
        if (!poll.ok) {
          const j = await poll.json().catch(() => ({}));
          throw new Error(j.error?.message ?? `HTTP ${poll.status}`);
        }
        done = poll;
      }
      const url = URL.createObjectURL(await done.blob());
      setLast({ content: `[image: ${prompt}] (${Math.round((Date.now() - started) / 1000)}s on a c0mpute CPU worker)`, image: url });
    } catch (e) {
      if (!(e instanceof DOMException && e.name === "AbortError")) {
        setFailure(e instanceof Error ? e.message : String(e));
      }
      setLast({ content: "// image not drawn" });
    } finally {
      setBusy(false);
      abort.current = null;
    }
  }

  async function composeMusic(convo: Convo, text: string, music: MusicCommand) {
    const id = convo.id;
    const title = convo.messages.length === 0 ? text.slice(0, 60) : convo.title;
    setInput("");
    setFailure(null);
    setBusy(true);
    const started = Date.now();
    const setLast = (patch: Partial<Msg>) =>
      update(id, (c) => {
        const msgs = c.messages.slice();
        msgs[msgs.length - 1] = { ...msgs[msgs.length - 1], ...patch };
        return { ...c, messages: msgs, updated: Date.now() };
      });
    setConvos((all) => {
      const exists = all.some((c) => c.id === id);
      const base = exists ? all : [convo, ...all];
      const next = base.map((c) =>
        c.id === id
          ? {
              ...c,
              title,
              messages: [
                ...c.messages,
                { role: "user" as const, content: text, imageTurn: true },
                { role: "assistant" as const, content: "// composing with ACE-Step 1.5 on a c0mpute worker, a few minutes…", imageTurn: true },
              ],
              updated: Date.now(),
            }
          : c,
      );
      save(next);
      return next;
    });
    const ctrl = new AbortController();
    abort.current = ctrl;
    try {
      const res = await fetch("/api/v1/music", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ prompt: music.prompt, lyrics: music.lyrics }),
        signal: ctrl.signal,
      });
      const start = await res.json().catch(() => ({}));
      if (res.status !== 202 || !start.poll) throw new Error(start.error?.message ?? `HTTP ${res.status}`);
      // Infernet may write lyrics from the brief first; keep what it wrote.
      let written: { caption: string; lyrics: string } | null = null;
      const deadline = Date.now() + 45 * 60_000;
      let done: Response | null = null;
      while (!done) {
        if (Date.now() > deadline) throw new Error("song timed out");
        await new Promise((r) => setTimeout(r, 5000));
        if (ctrl.signal.aborted) throw new DOMException("aborted", "AbortError");
        const poll = await fetch(start.poll, { cache: "no-store", signal: ctrl.signal });
        if (poll.status === 202) {
          const p = await poll.json().catch(() => ({}));
          const secs = Math.round((Date.now() - started) / 1000);
          if (typeof p.caption === "string") written = { caption: p.caption, lyrics: typeof p.lyrics === "string" ? p.lyrics : "" };
          setLast({
            content:
              p.stage === "writing"
                ? `// writing lyrics and a style from your brief… ${secs}s`
                : `// composing with ACE-Step 1.5 on a c0mpute worker… ${secs}s${written?.lyrics ? `\n\n${written.lyrics}` : ""}`,
          });
          continue;
        }
        if (!poll.ok) {
          const j = await poll.json().catch(() => ({}));
          throw new Error(j.error?.message ?? `HTTP ${poll.status}`);
        }
        done = poll;
      }
      const blob = await done.blob();
      const url = URL.createObjectURL(blob);
      setLast({
        content: `[music: ${(written?.caption ?? music.prompt).slice(0, 300)}${written?.lyrics || music.lyrics ? " (with lyrics)" : ", instrumental"}] (${Math.round((Date.now() - started) / 1000)}s on a c0mpute worker)${written?.lyrics ? `\n\n${written.lyrics}` : ""}`,
        audio: url,
        audioType: blob.type || "audio/mpeg",
      });
    } catch (e) {
      if (!(e instanceof DOMException && e.name === "AbortError")) {
        setFailure(e instanceof Error ? e.message : String(e));
      }
      setLast({ content: "// song not made" });
    } finally {
      setBusy(false);
      abort.current = null;
    }
  }

  async function send() {
    const text = input.trim();
    if (!text || busy) return;
    let convo = active;
    if (!convo) {
      convo = newConvo("auto");
      const c = convo;
      setConvos((all) => {
        const next = [c, ...all];
        save(next);
        return next;
      });
      setActiveId(c.id);
    }
    const id = convo.id;
    const music = parseMusicCommand(text);
    if (music) {
      await composeMusic(convo, text, music);
      return;
    }
    const imagePrompt = parseImageCommand(text);
    if (imagePrompt) {
      await drawImage(convo, text, imagePrompt);
      return;
    }
    const history: Msg[] = [...convo.messages, { role: "user", content: text }];
    const title = convo.messages.length === 0 ? text.slice(0, 60) : convo.title;
    setInput("");
    setFailure(null);
    setBusy(true);
    // Insert the user turn plus an empty assistant turn that streams in.
    setConvos((all) => {
      const exists = all.some((c) => c.id === id);
      const base = exists ? all : [convo!, ...all];
      const next = base.map((c) =>
        c.id === id ? { ...c, title, messages: [...history, { role: "assistant" as const, content: "" }], updated: Date.now() } : c,
      );
      save(next);
      return next;
    });
    const ctrl = new AbortController();
    abort.current = ctrl;
    try {
      const res = await fetch("/api/v1/chat/completions", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          model: convo.model,
          messages: textHistory(history).map(({ role, content }) => ({ role, content })),
          stream: true,
        }),
        signal: ctrl.signal,
      });
      if (!res.ok) {
        const j = await res.json().catch(() => ({}));
        throw new Error(j.error?.message ?? `HTTP ${res.status}`);
      }
      await readStream(res, (t) =>
        update(id, (c) => {
          const msgs = c.messages.slice();
          const last = msgs[msgs.length - 1];
          msgs[msgs.length - 1] = { ...last, content: last.content + t };
          return { ...c, messages: msgs, updated: Date.now() };
        }),
      );
    } catch (e) {
      if (!(e instanceof DOMException && e.name === "AbortError")) {
        setFailure(e instanceof Error ? e.message : String(e));
        // Drop an empty assistant turn so a retry starts clean.
        update(id, (c) => {
          const last = c.messages[c.messages.length - 1];
          return last?.role === "assistant" && !last.content ? { ...c, messages: c.messages.slice(0, -1) } : c;
        });
      }
    } finally {
      setBusy(false);
      abort.current = null;
    }
  }

  async function signOut() {
    await fetch("/api/auth/logout", { method: "POST" });
    setMe(null);
  }

  if (me === undefined) return <div className="max-w-5xl mx-auto px-6 py-16 text-sm text-[var(--color-dim)]">loading…</div>;

  if (!me) {
    return (
      <div className="max-w-3xl mx-auto px-6 py-16 space-y-10">
        <header className="space-y-2">
          <h1 className="text-2xl font-bold accent">chat</h1>
          <p className="comment">// open models on the c0mpute network, in the browser, the CLI, any OpenAI client, or MCP</p>
        </header>
        {error && <p className="border border-[var(--color-warn)] text-[var(--color-warn)] p-3 text-sm">{error}</p>}
        <section className="space-y-4 text-sm leading-7">
          <p>
            Every answer is generated on a provider node (Infernet, on c0mpute), not a hosted API. Sign-in is CoinPay
            only, and your CoinPay account needs a DID: that DID is your c0mpute chat identity.
          </p>
          <a
            href="/api/auth/coinpay/login?next=/chat"
            className="inline-block !border border-[var(--color-accent)] px-4 py-2 accent hover:bg-[var(--color-accent-dim)]"
          >
            Sign in with CoinPay
          </a>
          <p className="text-[var(--color-dim)]">
            No DID yet? Sign in at <a href="https://coinpayportal.com">coinpayportal.com</a> and create one, or run{" "}
            <code>c0mpute coinpay reputation did setup</code>.
          </p>
        </section>
        <Surfaces />
        <Models models={models} />
      </div>
    );
  }

  return (
    <div className="max-w-6xl mx-auto px-4 py-6 grid gap-4 md:grid-cols-[220px_1fr] min-h-[calc(100vh-10rem)]">
      <aside className="space-y-3 text-sm min-w-0">
        <button
          className="w-full border border-[var(--color-accent)] px-3 py-2 accent hover:bg-[var(--color-accent-dim)]"
          onClick={() => {
            const c = newConvo(active?.model ?? "auto");
            setConvos((all) => {
              const next = [c, ...all];
              save(next);
              return next;
            });
            setActiveId(c.id);
            setTab("chat");
          }}
        >
          + new chat
        </button>
        <nav className="space-y-1 max-h-[50vh] overflow-y-auto">
          {convos.map((c) => (
            <div key={c.id} className="group flex items-center gap-1">
              <button
                onClick={() => {
                  setActiveId(c.id);
                  setTab("chat");
                }}
                className={`flex-1 truncate text-left px-2 py-1 ${c.id === activeId && tab === "chat" ? "accent bg-[var(--color-card)]" : "text-[var(--color-dim)] hover:text-[var(--color-fg)]"}`}
              >
                {c.title}
              </button>
              <button
                aria-label="delete chat"
                className="opacity-0 group-hover:opacity-100 px-1 text-[var(--color-dim)] hover:text-[var(--color-warn)]"
                onClick={() =>
                  setConvos((all) => {
                    const next = all.filter((x) => x.id !== c.id);
                    save(next);
                    if (activeId === c.id) setActiveId(next[0]?.id ?? null);
                    return next;
                  })
                }
              >
                ×
              </button>
            </div>
          ))}
        </nav>
        <div className="border-t border-[var(--color-rule)] pt-3 space-y-2 text-xs text-[var(--color-dim)]">
          <button onClick={() => setTab("connect")} className={tab === "connect" ? "accent" : "hover:text-[var(--color-fg)]"}>
            CLI / API / MCP keys
          </button>
          <p className="break-all" title={me.did}>
            {me.name ? `${me.name} · ` : ""}
            {me.did}
          </p>
          <BalanceLine balance={me.balance ?? null} />
          <button onClick={signOut} className="hover:text-[var(--color-fg)]">
            sign out
          </button>
        </div>
      </aside>

      {tab === "connect" ? (
        <Connect />
      ) : (
        <section className="flex flex-col min-w-0 border border-[var(--color-rule)] bg-[var(--color-panel)]">
          <div className="flex items-center gap-2 border-b border-[var(--color-rule)] px-3 py-2 text-xs">
            <label className="text-[var(--color-dim)]" htmlFor="model">
              model
            </label>
            <select
              id="model"
              className="bg-[var(--color-card)] border border-[var(--color-rule)] px-2 py-1 max-w-full"
              value={active?.model ?? "auto"}
              onChange={(e) => {
                if (active) update(active.id, (c) => ({ ...c, model: e.target.value }));
              }}
            >
              <option value="auto">auto (any live node)</option>
              {models.map((m) => (
                <option key={m} value={m}>
                  {m}
                </option>
              ))}
            </select>
            <span className="ml-auto text-[var(--color-dim)]">{models.length} models online</span>
          </div>
          <div className="flex-1 overflow-y-auto p-4 space-y-4 text-sm leading-7">
            {(!active || active.messages.length === 0) && (
              <p className="comment">// ask anything. Answers stream from a provider node on the network. /image &lt;prompt&gt; draws a picture. /music &lt;style&gt; makes a song (add a line &quot;lyrics:&quot; then the words for vocals).</p>
            )}
            {active?.messages.map((m, i) => (
              <div key={i} className={m.role === "user" ? "text-[var(--color-fg)]" : "text-[var(--color-fg)] opacity-90"}>
                <div className={`text-xs mb-1 ${m.role === "user" ? "accent" : "text-[var(--color-dim)]"}`}>
                  {m.role === "user" ? "> you" : "< c0mpute"}
                </div>
                <div className="whitespace-pre-wrap break-words">
                  {m.content || (busy && i === active.messages.length - 1 ? "▍" : "")}
                </div>
                {m.image && (
                  <figure className="mt-2">
                    {/* eslint-disable-next-line @next/next/no-img-element -- a session blob: URL */}
                    <img src={m.image} alt={m.content} className="max-w-full w-[640px] border border-[var(--color-rule)]" />
                    <a href={m.image} download="c0mpute-image.png" className="text-xs underline text-[var(--color-dim)] hover:text-[var(--color-fg)]">
                      download
                    </a>
                  </figure>
                )}
                {m.audio && (
                  <figure className="mt-2">
                    <audio controls src={m.audio} className="w-full max-w-[640px]" />
                    <a
                      href={m.audio}
                      download={`c0mpute-music.${audioExtension(m.audioType ?? "")}`}
                      className="text-xs underline text-[var(--color-dim)] hover:text-[var(--color-fg)]"
                    >
                      download
                    </a>
                  </figure>
                )}
              </div>
            ))}
            {failure && <p className="text-[var(--color-warn)] text-xs">error: {failure}</p>}
            <div ref={bottom} />
          </div>
          <form
            className="border-t border-[var(--color-rule)] p-3 flex gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              send();
            }}
          >
            <textarea
              value={input}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  send();
                }
              }}
              rows={2}
              placeholder="message, /image <prompt>, or /music <style> (Enter to send, Shift+Enter for a new line)"
              className="flex-1 resize-y bg-[var(--color-card)] border border-[var(--color-rule)] p-2 text-sm"
            />
            {busy ? (
              <button type="button" onClick={() => abort.current?.abort()} className="border border-[var(--color-warn)] px-4 text-[var(--color-warn)]">
                stop
              </button>
            ) : (
              <button type="submit" className="border border-[var(--color-accent)] px-4 accent hover:bg-[var(--color-accent-dim)]">
                send
              </button>
            )}
          </form>
        </section>
      )}
    </div>
  );
}

function Connect() {
  const [keys, setKeys] = useState<Key[]>([]);
  const [fresh, setFresh] = useState<string | null>(null);
  const [label, setLabel] = useState("");
  const refresh = () =>
    fetch("/api/v1/keys")
      .then((r) => r.json())
      .then((j) => setKeys(j.keys ?? []));
  useEffect(() => {
    refresh();
  }, []);
  return (
    <section className="space-y-8 min-w-0 text-sm leading-7">
      <div className="space-y-3">
        <h2 className="font-semibold">[ API keys ]</h2>
        <p className="text-[var(--color-dim)]">
          Keys act as you for the CLI, MCP clients and OpenAI-compatible tools. <code>c0mpute chat login</code> makes
          one for you.
        </p>
        <form
          className="flex gap-2"
          onSubmit={async (e) => {
            e.preventDefault();
            const r = await fetch("/api/v1/keys", {
              method: "POST",
              headers: { "content-type": "application/json" },
              body: JSON.stringify({ label: label || "api key" }),
            });
            const j = await r.json();
            setFresh(j.key ?? null);
            setLabel("");
            refresh();
          }}
        >
          <input
            value={label}
            onChange={(e) => setLabel(e.target.value)}
            placeholder="label (e.g. laptop, claude code)"
            className="flex-1 bg-[var(--color-card)] border border-[var(--color-rule)] px-2 py-1"
          />
          <button className="border border-[var(--color-accent)] px-3 accent">create key</button>
        </form>
        {fresh && (
          <div className="space-y-1">
            <p className="text-[var(--color-warn)] text-xs">Copy it now. It is not shown again.</p>
            <CodeBlock>{fresh}</CodeBlock>
          </div>
        )}
        <ul className="space-y-1">
          {keys.map((k) => (
            <li key={k.id} className="flex items-center gap-3 text-xs">
              <code>{k.prefix}…</code>
              <span>{k.label}</span>
              <span className="text-[var(--color-dim)]">
                {k.last_used_at ? `used ${new Date(k.last_used_at).toLocaleDateString()}` : "never used"}
              </span>
              <button
                className="ml-auto text-[var(--color-dim)] hover:text-[var(--color-warn)]"
                onClick={async () => {
                  await fetch(`/api/v1/keys?id=${k.id}`, { method: "DELETE" });
                  refresh();
                }}
              >
                revoke
              </button>
            </li>
          ))}
          {keys.length === 0 && <li className="text-xs text-[var(--color-dim)]">no keys yet</li>}
        </ul>
      </div>
      <Surfaces />
    </section>
  );
}

function Surfaces() {
  return (
    <section className="space-y-6 text-sm leading-7 min-w-0">
      <div className="space-y-2">
        <h2 className="font-semibold">[ CLI ]</h2>
        <CodeBlock>{`$ curl -fsSL https://c0mpute.com/install.sh | sh
$ c0mpute chat login            # opens c0mpute.com, approve with CoinPay
$ c0mpute chat "explain LoRA in two sentences"
$ c0mpute chat                  # interactive session
$ c0mpute chat models
$ cat notes.md | c0mpute chat --system "summarize" --model qwen2.5:7b`}</CodeBlock>
      </div>
      <div className="space-y-2">
        <h2 className="font-semibold">[ API (OpenAI-compatible) ]</h2>
        <CodeBlock>{`$ curl https://c0mpute.com/v1/chat/completions \\
    -H "Authorization: Bearer $C0MPUTE_API_KEY" -H "content-type: application/json" \\
    -d '{"model":"auto","messages":[{"role":"user","content":"hi"}],"stream":true}'

# any OpenAI SDK: base_url=https://c0mpute.com/v1  api_key=c0k_...`}</CodeBlock>
      </div>
      <div className="space-y-2">
        <h2 className="font-semibold">[ MCP ]</h2>
        <CodeBlock>{`$ claude mcp add --transport http c0mpute https://c0mpute.com/api/mcp \\
    --header "Authorization: Bearer $C0MPUTE_API_KEY"

# stdio clients: { "command": "c0mpute", "args": ["chat", "mcp"] }
# tools: chat, list_models, whoami`}</CodeBlock>
      </div>
    </section>
  );
}

function Models({ models }: { models: string[] }) {
  if (models.length === 0) return null;
  return (
    <section className="space-y-2 text-sm">
      <h2 className="font-semibold">[ live models ]</h2>
      <ul className="text-xs text-[var(--color-dim)] columns-1 sm:columns-2">
        {models.map((m) => (
          <li key={m} className="break-all">
            {m}
          </li>
        ))}
      </ul>
    </section>
  );
}
