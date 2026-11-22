"use client";

import { useCallback, useEffect, useState } from "react";

// Mirrors /api/v1/live (kept loose: this is a client file).
type Stream = { id: string; name: string; title: string; visibility: string; live: boolean; tier: string; watch_url: string; playback_url: string; rtmp_url: string };
type Me = { account: { label: string }; balance_usd: number; tier: string; usd_per_gb: number; free_tier: { max_height: number; max_viewers: number }; rtmp_url: string; top_up: string };
type Fresh = { id: string; stream_key: string; publish_url: string };

async function call(path: string, init?: RequestInit) {
  const r = await fetch(path, { cache: "no-store", ...init, headers: { "content-type": "application/json", ...(init?.headers ?? {}) } });
  const body = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(body?.error?.message || `HTTP ${r.status}`);
  return body;
}

function Copy({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex items-start gap-2">
      <pre className="flex-1 !border border-[var(--color-rule)] px-3 py-2 text-xs whitespace-pre-wrap break-all">{text}</pre>
      <button
        type="button"
        className="!border border-[var(--color-rule)] px-2 py-1 text-xs text-[var(--color-dim)]"
        onClick={() =>
          navigator.clipboard?.writeText(text).then(() => {
            setCopied(true);
            setTimeout(() => setCopied(false), 1500);
          })
        }
      >
        {copied ? "copied" : "copy"}
      </button>
    </div>
  );
}

export function LiveDashboard() {
  const [me, setMe] = useState<Me | null>(null);
  const [streams, setStreams] = useState<Stream[]>([]);
  const [fresh, setFresh] = useState<Fresh | null>(null);
  const [name, setName] = useState("");
  const [unlisted, setUnlisted] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [signedOut, setSignedOut] = useState(false);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      const [m, s] = await Promise.all([call("/api/v1/live/me"), call("/api/v1/live/streams")]);
      setMe(m);
      setStreams(s.streams ?? []);
    } catch (e) {
      if (/Bearer|401|Sign in/i.test(String(e))) setSignedOut(true);
      else setError(e instanceof Error ? e.message : String(e));
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load]);

  const act = async (f: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await f();
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  if (signedOut) {
    return (
      <p>
        <a href="/signin?next=/dashboard/live">Sign in</a> to create a stream.
      </p>
    );
  }

  return (
    <div className="space-y-8">
      {me && (
        <section className="space-y-1 text-sm">
          <p>
            Streaming as <b>{me.account.label}</b>. Tier: <b>{me.tier}</b>. Balance: <b>${me.balance_usd.toFixed(2)}</b>.
          </p>
          <p className="comment">
            {me.tier === "paid"
              ? `// relays are paid $${me.usd_per_gb} per GB they carry for you, from your balance`
              : `// free tier: up to ${me.free_tier.max_height}p and ${me.free_tier.max_viewers} viewers at once; relays are not paid`}{" "}
            <a href={me.top_up}>top up</a>
          </p>
        </section>
      )}

      <form
        className="flex flex-wrap items-end gap-3 text-sm"
        onSubmit={(e) => {
          e.preventDefault();
          void act(async () => {
            const s = await call("/api/v1/live/streams", { method: "POST", body: JSON.stringify({ name, visibility: unlisted ? "unlisted" : "public" }) });
            setFresh({ id: s.id, stream_key: s.stream_key, publish_url: s.publish_url });
            setName("");
          });
        }}
      >
        <label className="flex flex-col gap-1">
          <span>New stream name</span>
          <input className="!border border-[var(--color-rule)] bg-transparent px-2 py-1" value={name} onChange={(e) => setName(e.target.value)} placeholder="friday show" maxLength={63} required />
        </label>
        <label className="flex items-center gap-2">
          <input type="checkbox" checked={unlisted} onChange={(e) => setUnlisted(e.target.checked)} /> unlisted (link only)
        </label>
        <button type="submit" disabled={busy} className="!border border-[var(--color-accent)] accent px-3 py-1">
          create stream
        </button>
      </form>

      {error && (
        <p role="alert" className="text-sm text-red-400">
          {error}
        </p>
      )}

      {fresh && (
        <section className="space-y-2 text-sm !border border-[var(--color-accent)] p-3">
          <p>
            <b>Stream key (shown once).</b> In OBS or PairUX use this server and key, or the full URL:
          </p>
          <Copy text={me?.rtmp_url ?? "rtmp://live.c0mpute.com/live"} />
          <Copy text={fresh.stream_key} />
          <Copy text={fresh.publish_url} />
        </section>
      )}

      <section className="space-y-4">
        <h2 className="font-bold">your streams</h2>
        {streams.length === 0 && <p className="comment">// none yet</p>}
        {streams.map((s) => (
          <div key={s.id} className="space-y-2 text-sm !border border-[var(--color-rule)] p-3">
            <p>
              <b>{s.title}</b> <span className="comment">// {s.name} · {s.visibility} · {s.live ? "live now" : "off air"}</span>
            </p>
            <Copy text={s.watch_url} />
            <div className="flex gap-3">
              <button
                type="button"
                disabled={busy}
                className="!border border-[var(--color-rule)] px-2 py-1 text-xs"
                onClick={() =>
                  void act(async () => {
                    const k = await call(`/api/v1/live/streams/${s.id}/key`, { method: "POST" });
                    setFresh({ id: s.id, stream_key: k.stream_key, publish_url: k.publish_url });
                  })
                }
              >
                new key
              </button>
              <button
                type="button"
                disabled={busy}
                className="!border border-[var(--color-rule)] px-2 py-1 text-xs text-red-400"
                onClick={() => {
                  if (confirm(`Delete "${s.title}"? Its key and watch link stop working.`)) void act(async () => void (await call(`/api/v1/live/streams/${s.id}`, { method: "DELETE" })));
                }}
              >
                delete
              </button>
            </div>
          </div>
        ))}
      </section>
    </div>
  );
}
