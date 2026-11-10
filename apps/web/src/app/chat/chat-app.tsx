"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { CodeBlock } from "@/components/CodeBlock";
import { parseImageCommand, textHistory } from "@/lib/chat/images";
import { audioExtension, describeNodeProgress, parseMusicCommand, sanitizeNodeProgress, type MusicCommand } from "@/lib/chat/music";
import { describeAlbumStage, sanitizeAlbum, trackFileBase, parseAlbumCommand, type AlbumCommand, type AlbumTrack } from "@/lib/chat/album";
import { describePromoProgress, detectPromoRequest, parsePromoCommand, type PromoCommand, type PromoScene } from "@/lib/chat/promo";
import { collectPushFiles, parseConnectCommand, parsePushCommand, type ConnectCommand, type PushCommand } from "@/lib/chat/push-shared";

const PUSH_NODE = "c0_push_node";
const SPIN = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

/** A terminal spinner: one braille frame every 80 ms. */
function Spinner() {
  const [i, setI] = useState(0);
  useEffect(() => {
    const t = setInterval(() => setI((n) => (n + 1) % SPIN.length), 80);
    return () => clearInterval(t);
  }, []);
  return (
    <span className="accent inline-block w-[1ch]" aria-hidden>
      {SPIN[i]}
    </span>
  );
}

/** Seconds since a push started, ticking once a second. */
function Elapsed({ since }: { since: number }) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);
  return <span className="text-[var(--color-dim)]">{clock(Math.max(0, Math.round((now - since) / 1000)))}</span>;
}

const PUSH_STAGE: Record<string, string> = {
  queued: "waiting for your node to pick it up",
  publishing: "your node is publishing",
  deleting: "your node is taking the site down",
};

function PushCard({ p }: { p: PushView }) {
  const running = !PUSH_FINAL.has(p.status);
  return (
    <div className="mt-2 border border-[var(--color-rule)] p-3 text-xs space-y-1 max-w-[640px]" aria-live="polite">
      <p className="flex items-center gap-2">
        {running ? <Spinner /> : <span className={p.status === "failed" ? "text-[var(--color-warn)]" : "accent"}>{p.status === "failed" ? "✗" : "✓"}</span>}
        <span>
          {p.action === "rm" ? "rm" : "push"} hosting <b>{p.name}</b>
          {p.node ? <span className="text-[var(--color-dim)]"> via {p.node}</span> : null}
        </span>
        <span className="ml-auto">{running ? <Elapsed since={p.started} /> : null}</span>
      </p>
      {running && (
        <p className="text-[var(--color-dim)]">
          {PUSH_STAGE[p.status] ?? p.status}
          {p.progress ? <> · <span className="text-[var(--color-fg)]">{p.progress}</span></> : null}
        </p>
      )}
      {p.action === "publish" && <p className="text-[var(--color-dim)]">{p.files} files · paid hosts from the d1sks marketplace (--disks auto)</p>}
      {p.status === "live" && p.url && (
        <p>
          live:{" "}
          <a href={p.url} target="_blank" rel="noreferrer" className="accent underline break-all">
            {p.url}
          </a>
          {p.copies != null ? <span className="text-[var(--color-dim)]"> · {p.copies} copies</span> : null}
        </p>
      )}
      {p.status === "deleted" && <p className="text-[var(--color-dim)]">site removed from the network</p>}
      {p.error && <p className="text-[var(--color-warn)] break-words">{p.error}</p>}
    </div>
  );
}

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
  /** The music job a song message came from, so late cover art finds it. */
  musicJob?: string;
  /** A promo video: storyboard frames while it renders, then the MP4 (served by /api/v1/promo/<id>). */
  promo?: { id: string; scenes: PromoScene[]; vertical: boolean; video?: string; captions?: string; duration?: number; title?: string; motion?: string };
  /** An album: band, title, cover and tracks (each plays from /api/v1/music/<track id>), then the zip. */
  album?: { id: string; band: string | null; album: string | null; genre: string | null; concept: string | null; cover: string | null; zip: string | null; tracks: AlbumTrack[] };
  /** The image job a picture came from, so /push can publish it later. */
  imageJob?: string;
  /** A `/push hosting` turn: polled live until the site is up (or gone). */
  push?: PushView;
};
type PushView = {
  id: string;
  name: string;
  action: "publish" | "rm";
  status: string;
  progress: string | null;
  node: string | null;
  files: number;
  url: string | null;
  site_id: string | null;
  copies: number | null;
  error: string | null;
  started: number;
};
const PUSH_FINAL = new Set(["live", "failed", "deleted"]);
type Convo = { id: string; title: string; model: string; messages: Msg[]; updated: number };
type Balance = { spendable_usd: string; earned_spendable_usd: string; prepaid_available_usd: string };
type Me = { id: string; did: string; name: string | null; balance?: Balance | null };
type Key = { id: string; label: string; prefix: string; created_at: string; last_used_at: string | null };

const STORE = "c0_chats_v1";
// How long the chat keeps polling a media job: the network queues a job up to
// 2 h for a free worker, then renders it.
const QUEUE_DEADLINE_MS = 3 * 60 * 60_000;
const clock = (s: number) => (s < 120 ? `${s}s` : `${Math.floor(s / 60)} min`);

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
      // The network queues a job until a worker is free (up to 2 h), then draws it.
      const deadline = Date.now() + QUEUE_DEADLINE_MS;
      let done: Response | null = null;
      while (!done) {
        if (Date.now() > deadline) throw new Error("image timed out");
        await new Promise((r) => setTimeout(r, 4000));
        if (ctrl.signal.aborted) throw new DOMException("aborted", "AbortError");
        const poll = await fetch(start.poll, { cache: "no-store", signal: ctrl.signal });
        if (poll.status === 202) {
          const p = await poll.json().catch(() => ({}));
          const secs = Math.round((Date.now() - started) / 1000);
          setLast({
            content:
              p.stage === "queued"
                ? `// queued for a c0mpute worker${typeof p.queue_position === "number" ? ` (#${p.queue_position} in line)` : ""}, it starts when one is free… ${clock(secs)}`
                : `// drawing on a c0mpute CPU worker… ${clock(secs)}`,
          });
          continue;
        }
        if (!poll.ok) {
          const j = await poll.json().catch(() => ({}));
          throw new Error(j.error?.message ?? `HTTP ${poll.status}`);
        }
        done = poll;
      }
      const url = URL.createObjectURL(await done.blob());
      const imageJob = String(start.poll).split("/").pop() || undefined;
      setLast({ content: `[image: ${prompt}] (${Math.round((Date.now() - started) / 1000)}s on a c0mpute CPU worker)`, image: url, imageJob });
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
      let written: { caption: string; lyrics: string; title?: string | null } | null = null;
      let coverUrl: string | null = null;
      const deadline = Date.now() + QUEUE_DEADLINE_MS;
      let done: Response | null = null;
      while (!done) {
        if (Date.now() > deadline) throw new Error("song timed out");
        await new Promise((r) => setTimeout(r, 5000));
        if (ctrl.signal.aborted) throw new DOMException("aborted", "AbortError");
        const poll = await fetch(start.poll, { cache: "no-store", signal: ctrl.signal });
        if (poll.status === 202) {
          const p = await poll.json().catch(() => ({}));
          const secs = Math.round((Date.now() - started) / 1000);
          if (typeof p.caption === "string") written = { caption: p.caption, lyrics: typeof p.lyrics === "string" ? p.lyrics : "", title: typeof p.title === "string" ? p.title : null };
          // Cover art is drawn in parallel and usually lands before the song.
          if (p.cover === "ready" && !coverUrl) {
            const c = await fetch(`${start.poll}/cover`, { cache: "no-store", signal: ctrl.signal }).catch(() => null);
            if (c?.ok) {
              coverUrl = URL.createObjectURL(await c.blob());
              setLast({ image: coverUrl });
            }
          }
          const node = sanitizeNodeProgress(p.node);
          const head =
            p.stage === "writing"
              ? `// writing lyrics and a style from your brief… ${secs}s`
              : `// ${node ? describeNodeProgress(node) : `composing with ACE-Step 1.5 on a c0mpute worker…`} · ${secs}s total${
                  typeof p.duration === "number" ? ` · ${p.duration}s song` : ""
                }`;
          setLast({ content: `${head}${written?.lyrics ? `\n\n${written.lyrics}` : ""}` });
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
        ...(coverUrl ? { image: coverUrl } : {}),
        content: `${written?.title ? `"${written.title}" ` : ""}[music: ${(written?.caption ?? music.prompt).slice(0, 300)}${written?.lyrics || music.lyrics ? " (with lyrics)" : ", instrumental"}] (${Math.round((Date.now() - started) / 1000)}s on a c0mpute worker)${written?.lyrics ? `\n\n${written.lyrics}` : ""}`,
        audio: url,
        audioType: blob.type || "audio/mpeg",
        musicJob: start.id,
      });
      // The song can beat its cover (image workers queue separately): keep
      // looking for the cover and attach it to this song's message.
      if (!coverUrl) {
        const jobId: string = start.id;
        void (async () => {
          const until = Date.now() + 8 * 60_000;
          while (Date.now() < until) {
            await new Promise((r) => setTimeout(r, 6000));
            const c = await fetch(`${start.poll}/cover`, { cache: "no-store" }).catch(() => null);
            if (!c?.ok) continue;
            const late = URL.createObjectURL(await c.blob());
            update(id, (cv) => ({ ...cv, messages: cv.messages.map((m) => (m.musicJob === jobId ? { ...m, image: late } : m)) }));
            return;
          }
        })();
      }
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

  async function makeAlbum(convo: Convo, text: string, cmd: AlbumCommand) {
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
                { role: "assistant" as const, content: "// naming the band and writing the album…", imageTurn: true },
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
      const res = await fetch("/api/v1/album", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ prompt: cmd.prompt }),
        signal: ctrl.signal,
      });
      const start = await res.json().catch(() => ({}));
      if (res.status !== 202 || !start.poll) throw new Error(start.error?.message ?? `HTTP ${res.status}`);
      const note = start.limited ? `\n// only ${start.tracks} free songs left today, so this album has ${start.tracks} tracks` : "";
      // Planning, lyrics, then every track through the network's queue.
      const deadline = Date.now() + 2 * QUEUE_DEADLINE_MS;
      for (;;) {
        if (Date.now() > deadline) throw new Error("album timed out");
        await new Promise((r) => setTimeout(r, 5000));
        if (ctrl.signal.aborted) throw new DOMException("aborted", "AbortError");
        const poll = await fetch(start.poll, { cache: "no-store", signal: ctrl.signal });
        const p = await poll.json().catch(() => ({}));
        if (poll.status !== 202 && poll.status !== 200) throw new Error(p.error?.message ?? `HTTP ${poll.status}`);
        const secs = Math.round((Date.now() - started) / 1000);
        const view = sanitizeAlbum({ ...p, cover: p.cover ? "ready" : null });
        const album = {
          id: start.id as string,
          band: view.band,
          album: view.album,
          genre: view.genre,
          concept: view.concept,
          cover: typeof p.cover === "string" ? p.cover : null,
          zip: typeof p.zip === "string" ? p.zip : null,
          tracks: view.tracks,
        };
        const head = album.album ? `"${album.album}" by ${album.band}` : "album";
        if (poll.status === 200) {
          const done = album.tracks.filter((t) => t.status === "done").length;
          setLast({ content: `${head}${album.genre ? ` · ${album.genre}` : ""} · ${done}/${album.tracks.length} tracks (${clock(secs)} on c0mpute workers)`, album });
          // The cover queues on the image workers separately and can land
          // after the songs: keep looking (up to 2 h). The zip picks it up
          // on the next download.
          if (!album.cover) {
            const albumId = album.id;
            const coverPath = `/api/v1/album/${albumId}/cover`;
            void (async () => {
              const until = Date.now() + 2 * 60 * 60_000;
              while (Date.now() < until) {
                await new Promise((r) => setTimeout(r, 30_000));
                const c = await fetch(coverPath, { cache: "no-store" }).catch(() => null);
                if (!c?.ok) continue;
                await c.body?.cancel().catch(() => {});
                update(id, (cv) => ({ ...cv, messages: cv.messages.map((m) => (m.album?.id === albumId ? { ...m, album: { ...m.album, cover: coverPath } } : m)) }));
                return;
              }
            })();
          }
          break;
        }
        setLast({ content: `// ${head} · ${describeAlbumStage(view)} · ${clock(secs)}${note}`, album });
      }
    } catch (e) {
      if (!(e instanceof DOMException && e.name === "AbortError")) {
        setFailure(e instanceof Error ? e.message : String(e));
      }
      setLast({ content: "// album not made" });
    } finally {
      setBusy(false);
      abort.current = null;
    }
  }

  async function makePromo(convo: Convo, text: string, req: PromoCommand) {
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
                { role: "assistant" as const, content: `// starting a ${req.style} promo for ${req.url}…`, imageTurn: true },
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
      const res = await fetch("/api/v1/promo", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ url: req.url, style: req.style, vertical: req.vertical, brief: req.brief }),
        signal: ctrl.signal,
      });
      const start = await res.json().catch(() => ({}));
      if (res.status !== 202 || !start.poll) throw new Error(start.error?.message ?? `HTTP ${res.status}`);
      const deadline = Date.now() + QUEUE_DEADLINE_MS;
      while (true) {
        if (Date.now() > deadline) throw new Error("promo timed out");
        await new Promise((r) => setTimeout(r, 4000));
        if (ctrl.signal.aborted) throw new DOMException("aborted", "AbortError");
        const poll = await fetch(start.poll, { cache: "no-store", signal: ctrl.signal });
        const p = await poll.json().catch(() => ({}));
        if (poll.status !== 200 && poll.status !== 202) throw new Error(p.error?.message ?? `HTTP ${poll.status}`);
        const secs = Math.round((Date.now() - started) / 1000);
        const head = `// ${p.title ?? `${req.style} promo`}${p.brand?.name ? ` for ${p.brand.name}` : ""} · ${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, "0")}`;
        const promo = { id: start.id as string, scenes: (p.scenes ?? []) as PromoScene[], vertical: Boolean(p.vertical), title: p.title };
        if (poll.status === 202) {
          setLast({ content: `${head}\n${describePromoProgress(p)}`, promo });
          continue;
        }
        setLast({
          content: `"${p.title ?? "promo"}" (${Math.round(p.duration ?? 0)}s ${req.style} promo for ${p.brand?.host ?? req.url}; made in ${secs}s on c0mpute workers${p.motion === "kenburns" ? ", keyframes with camera moves" : ", img2vid motion"})`,
          promo: { ...promo, video: p.video, captions: p.captions, duration: p.duration, motion: p.motion },
        });
        break;
      }
    } catch (e) {
      if (!(e instanceof DOMException && e.name === "AbortError")) {
        setFailure(e instanceof Error ? e.message : String(e));
      }
      setLast({ content: "// promo not made" });
    } finally {
      setBusy(false);
      abort.current = null;
    }
  }

  // ── /connect hosting + /push hosting ───────────────────────────────────────
  /** Append a user turn and an assistant turn; returns the assistant turn's index. */
  function addTurn(convo: Convo, text: string, reply: Msg): number {
    const title = convo.messages.length === 0 ? text.slice(0, 60) : convo.title;
    const index = convo.messages.length + 1;
    setInput("");
    setFailure(null);
    setConvos((all) => {
      const base = all.some((c) => c.id === convo.id) ? all : [convo, ...all];
      const next = base
        .map((c) =>
          c.id === convo.id ? { ...c, title, messages: [...c.messages, { role: "user" as const, content: text, imageTurn: true }, { ...reply, imageTurn: true }], updated: Date.now() } : c,
        )
        .sort((a, b) => b.updated - a.updated);
      save(next);
      return next;
    });
    return index;
  }

  /** Patch one message by index (a push keeps polling while the chat moves on). */
  const patchMsg = useCallback(
    (convoId: string, index: number, fn: (m: Msg) => Msg) =>
      update(convoId, (c) => {
        if (!c.messages[index]) return c;
        const msgs = c.messages.slice();
        msgs[index] = fn(msgs[index]);
        return { ...c, messages: msgs };
      }),
    [update],
  );

  const polling = useRef(new Set<string>());

  /** Poll a push every 2 s until it is live, failed or deleted, updating its card. */
  const pollPush = useCallback(
    async (convoId: string, index: number, view: PushView) => {
      const key = `${convoId}:${index}`;
      if (polling.current.has(key)) return;
      polling.current.add(key);
      const set = (patch: Partial<PushView>) => patchMsg(convoId, index, (m) => (m.push ? { ...m, push: { ...m.push, ...patch } } : m));
      const deadline = Date.now() + QUEUE_DEADLINE_MS;
      try {
        while (Date.now() < deadline) {
          await new Promise((r) => setTimeout(r, 2000));
          const res = await fetch(`/api/v1/hosting/pushes/${view.id}`, { cache: "no-store" }).catch(() => null);
          if (!res) continue;
          if (res.status === 404 && view.action === "rm") {
            set({ status: "deleted", progress: null, error: null });
            return;
          }
          const j = await res.json().catch(() => ({}));
          if (!res.ok) {
            if (res.status >= 500) continue;
            set({ status: "failed", error: j.error?.message ?? `HTTP ${res.status}` });
            return;
          }
          const p = j.push;
          if (view.action === "rm" && p.status === "live") {
            // The node reported the delete failed: the site is still up.
            set({ status: "failed", progress: null, error: p.error ?? "delete failed", url: p.url });
            return;
          }
          set({ status: p.status, progress: p.progress, node: p.node, url: p.url, site_id: p.site_id, copies: p.copies, error: p.error });
          if (view.action === "publish" && (p.status === "live" || p.status === "failed")) return;
        }
        set({ status: "failed", error: "gave up waiting; /push hosting ls shows where it got to" });
      } finally {
        polling.current.delete(key);
      }
    },
    [patchMsg],
  );

  // After a reload, pick unfinished pushes back up.
  useEffect(() => {
    for (const c of convos)
      c.messages.forEach((m, i) => {
        if (m.push && !PUSH_FINAL.has(m.push.status)) void pollPush(c.id, i, m.push);
      });
  }, [convos, pollPush]);

  async function runConnect(convo: Convo, text: string, cmd: ConnectCommand) {
    const index = addTurn(convo, text, { role: "assistant", content: "// looking up your hosting nodes…" });
    const say = (content: string) => patchMsg(convo.id, index, (m) => ({ ...m, content }));
    if (!me) return say("// sign in with CoinPay first (top right): pushes and nodes belong to your account.");
    if (cmd.error) return say(`// ${cmd.error}`);
    const res = await fetch("/api/v1/hosting/nodes", { cache: "no-store" }).catch(() => null);
    const j = res ? await res.json().catch(() => ({})) : {};
    if (!res?.ok) return say(`// could not list nodes: ${j.error?.message ?? "network error"}`);
    const nodes: { name: string; online: boolean; payments: string | null; version: string | null }[] = j.nodes ?? [];
    const howTo =
      "Link a node: on the machine that should publish, run\n\n  c0mpute chat login\n  c0mpute chat connect hosting --name <node>\n\nIt signs in with this CoinPay account, then publishes your pushes with its own key and pays hosts from its CoinPay wallet ([payments] rail = \"coinpay\"). Leave it running (or run it as a service).";
    let chosen: string | null = null;
    try {
      chosen = localStorage.getItem(PUSH_NODE);
    } catch {}
    if (cmd.node) {
      const n = nodes.find((x) => x.name === cmd.node);
      if (!n) return say(`// no node named ${cmd.node} on your account.\n\n${howTo}`);
      try {
        localStorage.setItem(PUSH_NODE, n.name);
      } catch {}
      chosen = n.name;
    }
    if (!nodes.length) return say(`// no hosting node linked yet.\n\n${howTo}`);
    const lines = nodes.map(
      (n) =>
        `${n.name === chosen ? "*" : " "} ${n.name.padEnd(20)} ${n.online ? "online " : "offline"}  ${n.payments === "coinpay" ? "pays via CoinPay" : "NO CoinPay payments: cannot push"}${n.version ? `  v${n.version}` : ""}`,
    );
    say(
      `${cmd.node ? `// /push hosting now publishes through ${cmd.node}.\n\n` : ""}your hosting nodes (* = used by /push):\n${lines.join("\n")}\n\n/connect hosting <node> picks one. /push hosting <name> publishes this chat's files as a site.\n\n${howTo}`,
    );
  }

  async function runPush(convo: Convo, text: string, cmd: PushCommand) {
    if (cmd.kind === "help") {
      addTurn(convo, text, {
        role: "assistant",
        content: `${cmd.error ? `// ${cmd.error}\n\n` : ""}/push hosting <name>     publish this chat's files (images, songs, albums, videos, transcript) as a site on the c0mpute network
/push hosting ls         your sites
/push hosting rm <name>  take a site down
/connect hosting [node]  link or pick the node that publishes them`,
      });
      return;
    }
    const index = addTurn(convo, text, { role: "assistant", content: "" });
    const say = (content: string) => patchMsg(convo.id, index, (m) => ({ ...m, content }));
    if (!me) return say("// sign in with CoinPay first (top right): pushes belong to your account and are paid through CoinPay.");

    if (cmd.kind === "ls") {
      say("// listing your sites…");
      const res = await fetch("/api/v1/hosting/pushes", { cache: "no-store" }).catch(() => null);
      const j = res ? await res.json().catch(() => ({})) : {};
      if (!res?.ok) return say(`// could not list: ${j.error?.message ?? "network error"}`);
      const ps: { name: string; status: string; node: string | null; url: string | null; files: unknown[]; copies: number | null; progress: string | null; error: string | null; updated_at: string }[] = j.pushes ?? [];
      if (!ps.length) return say("// no sites yet. /push hosting <name> publishes this chat's files.");
      return say(
        `your sites (${ps.length}):\n` +
          ps
            .map(
              (p) =>
                `${p.name.padEnd(24)} ${p.status.padEnd(10)} ${String(p.files.length).padStart(3)} files  ${p.node ?? "?"}  ${p.url ?? p.progress ?? ""}${p.error ? `\n${" ".repeat(25)}! ${p.error}` : ""}`,
            )
            .join("\n"),
      );
    }

    if (cmd.kind === "rm") {
      say(`// taking ${cmd.name} down…`);
      const res = await fetch(`/api/v1/hosting/pushes/${cmd.name}`, { method: "DELETE" }).catch(() => null);
      const j = res ? await res.json().catch(() => ({})) : {};
      if (!res?.ok) return say(`// ${j.error?.message ?? "network error"}`);
      const view: PushView = {
        id: (j.poll ?? "").split("/").pop() || cmd.name,
        name: cmd.name,
        action: "rm",
        status: j.status === "deleted" ? "deleted" : "deleting",
        progress: null,
        node: null,
        files: 0,
        url: null,
        site_id: null,
        copies: null,
        error: null,
        started: Date.now(),
      };
      patchMsg(convo.id, index, (m) => ({ ...m, content: `// rm hosting ${cmd.name}`, push: view }));
      if (view.status !== "deleted") void pollPush(convo.id, index, view);
      return;
    }

    const files = collectPushFiles(convo.title, convo.messages);
    say(`// sending ${files.length} files to your hosting node…`);
    let node: string | null = null;
    try {
      node = localStorage.getItem(PUSH_NODE);
    } catch {}
    const res = await fetch("/api/v1/hosting/pushes", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: cmd.name, node: node || undefined, files }),
    }).catch(() => null);
    const j = res ? await res.json().catch(() => ({})) : {};
    if (!res || res.status !== 202) {
      const msg = j.error?.message ?? "network error";
      return say(`// push failed: ${msg}${res?.status === 409 && /no hosting node/.test(msg) ? "\n\nTry /connect hosting." : ""}`);
    }
    const p = j.push;
    const view: PushView = {
      id: p.id,
      name: p.name,
      action: "publish",
      status: p.status,
      progress: p.progress,
      node: p.node,
      files: p.files.length,
      url: p.url,
      site_id: p.site_id,
      copies: p.copies,
      error: null,
      started: Date.now(),
    };
    patchMsg(convo.id, index, (m) => ({ ...m, content: `// push hosting ${cmd.name} (rev ${p.rev})`, push: view }));
    void pollPush(convo.id, index, view);
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
    const pushCmd = parsePushCommand(text);
    if (pushCmd) {
      await runPush(convo, text, pushCmd);
      return;
    }
    const connectCmd = parseConnectCommand(text);
    if (connectCmd) {
      await runConnect(convo, text, connectCmd);
      return;
    }
    const promo = parsePromoCommand(text) ?? detectPromoRequest(text);
    if (promo) {
      await makePromo(convo, text, promo);
      return;
    }
    const albumCmd = parseAlbumCommand(text);
    if (albumCmd) {
      await makeAlbum(convo, text, albumCmd);
      return;
    }
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
              <p className="comment">// ask anything. Answers stream from a provider node on the network. /image &lt;prompt&gt; draws a picture. /music &lt;style&gt; makes a song (add a line &quot;lyrics:&quot; then the words for vocals). /album &lt;idea&gt; makes a whole album: a band, 8 named songs with lyrics, cover art and a zip download. /promo &lt;url&gt; [pixar|anime|claymation|flat] [vertical] makes a promo video with narration and music. /push hosting &lt;name&gt; publishes everything this chat made as a site on the c0mpute network (/connect hosting links the node that publishes it).</p>
            )}
            {active?.messages.map((m, i) => (
              <div key={i} className={m.role === "user" ? "text-[var(--color-fg)]" : "text-[var(--color-fg)] opacity-90"}>
                <div className={`text-xs mb-1 ${m.role === "user" ? "accent" : "text-[var(--color-dim)]"}`}>
                  {m.role === "user" ? "> you" : "< c0mpute"}
                </div>
                <div className="whitespace-pre-wrap break-words">
                  {m.content || (busy && i === active.messages.length - 1 ? "▍" : "")}
                </div>
                {m.push && <PushCard p={m.push} />}
                {m.image && (
                  <figure className="mt-2">
                    {/* eslint-disable-next-line @next/next/no-img-element -- a session blob: URL */}
                    <img src={m.image} alt={m.content} className="max-w-full w-[640px] border border-[var(--color-rule)]" />
                    <a href={m.image} download="c0mpute-image.png" className="text-xs underline text-[var(--color-dim)] hover:text-[var(--color-fg)]">
                      download
                    </a>
                  </figure>
                )}
                {m.promo && (
                  <figure className="mt-2">
                    {m.promo.video && (
                      <video
                        controls
                        playsInline
                        preload="metadata"
                        src={m.promo.video}
                        className={`w-full border border-[var(--color-rule)] bg-black ${m.promo.vertical ? "max-w-[360px]" : "max-w-[640px]"}`}
                      >
                        {m.promo.captions && <track kind="captions" src={m.promo.captions} srcLang="en" label="English" />}
                      </video>
                    )}
                    {m.promo.scenes.some((s) => s.frame) && (
                      <div className={`mt-2 grid grid-cols-4 gap-1 ${m.promo.vertical ? "max-w-[360px]" : "max-w-[640px]"}`}>
                        {m.promo.scenes.map((s, n) =>
                          s.frame ? (
                            // eslint-disable-next-line @next/next/no-img-element -- same-origin keyframe
                            <img
                              key={n}
                              src={`/api/v1/promo/${m.promo!.id}/frame/${n}`}
                              alt={s.caption}
                              title={`${n + 1}. ${s.narration}`}
                              className={`w-full object-cover border border-[var(--color-rule)] ${m.promo!.vertical ? "aspect-[9/16]" : "aspect-video"}`}
                            />
                          ) : null,
                        )}
                      </div>
                    )}
                    {m.promo.video && (
                      <p className="text-xs text-[var(--color-dim)] flex gap-3">
                        <a href={`${m.promo.video}?download=1`} className="underline hover:text-[var(--color-fg)]">
                          download mp4
                        </a>
                        {m.promo.captions && (
                          <a href={m.promo.captions} className="underline hover:text-[var(--color-fg)]">
                            captions .srt
                          </a>
                        )}
                      </p>
                    )}
                  </figure>
                )}
                {m.album && (
                  <figure className="mt-2 max-w-[640px]">
                    <div className="flex flex-wrap items-start gap-3">
                      {m.album.cover ? (
                        // eslint-disable-next-line @next/next/no-img-element -- same-origin album cover
                        <img src={m.album.cover} alt={m.album.album ?? "album cover"} className="w-40 aspect-square object-cover border border-[var(--color-rule)]" />
                      ) : (
                        <div className="w-40 aspect-square border border-dashed border-[var(--color-rule)] flex items-center justify-center text-xs text-[var(--color-dim)]">cover…</div>
                      )}
                      <div className="min-w-0 flex-1 text-xs">
                        {m.album.album && <p className="text-sm accent">{m.album.album}</p>}
                        {m.album.band && <p>{m.album.band}</p>}
                        {m.album.genre && <p className="text-[var(--color-dim)]">{m.album.genre}</p>}
                        {m.album.concept && <p className="mt-1 text-[var(--color-dim)]">{m.album.concept}</p>}
                        {m.album.zip && (
                          <a href={m.album.zip} download className="mt-2 inline-block underline hover:text-[var(--color-fg)]">
                            download album .zip (mp3s, lyrics, cover)
                          </a>
                        )}
                      </div>
                    </div>
                    <ol className="mt-2 space-y-1 text-xs">
                      {m.album.tracks.map((t) => (
                        <li key={t.n} className="border border-[var(--color-rule)] px-2 py-1">
                          <div className="flex justify-between gap-2">
                            <span>
                              {String(t.n).padStart(2, "0")}. {t.title}
                            </span>
                            <span className={t.status === "failed" ? "text-[var(--color-warn)]" : "text-[var(--color-dim)]"}>
                              {t.status === "pending" && t.node
                                ? describeNodeProgress(t.node)
                                : t.status === "done"
                                  ? ""
                                  : t.status === "written"
                                    ? "lyrics written"
                                    : t.status === "pending"
                                      ? "recording"
                                      : t.status}
                            </span>
                          </div>
                          {t.status === "done" && t.id && (
                            <div className="flex items-center gap-2 mt-1">
                              <audio controls preload="none" src={`/api/v1/music/${t.id}`} className="w-full h-8" />
                              <a href={`/api/v1/music/${t.id}`} download={`${trackFileBase(t.n, t.title)}.mp3`} className="underline text-[var(--color-dim)] hover:text-[var(--color-fg)]">
                                mp3
                              </a>
                            </div>
                          )}
                          {t.error && <p className="text-[var(--color-warn)]">{t.error}</p>}
                          {t.lyrics && (
                            <details className="text-[var(--color-dim)]">
                              <summary className="cursor-pointer">lyrics</summary>
                              <p className="whitespace-pre-wrap">{t.lyrics}</p>
                            </details>
                          )}
                        </li>
                      ))}
                    </ol>
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
              placeholder="message, /image <prompt>, /music <style>, /album <idea>, /promo <url>, or /push hosting <name> (Enter to send, Shift+Enter for a new line)"
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
