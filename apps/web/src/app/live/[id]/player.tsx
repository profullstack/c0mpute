"use client";

import { useEffect, useRef, useState } from "react";

// hls.js, loaded only where the browser cannot play HLS itself (Safari and iOS
// can). Pinned; no npm dependency for one page.
const HLS_JS = "https://cdn.jsdelivr.net/npm/hls.js@1.5.17/dist/hls.min.js";

type HlsCtor = {
  new (cfg?: Record<string, unknown>): { loadSource(u: string): void; attachMedia(v: HTMLVideoElement): void; on(e: string, f: (e: unknown, d: { fatal?: boolean }) => void): void; destroy(): void };
  isSupported(): boolean;
  Events: { ERROR: string };
};

function loadHls(): Promise<HlsCtor | null> {
  const w = window as unknown as { Hls?: HlsCtor };
  if (w.Hls) return Promise.resolve(w.Hls);
  return new Promise((resolve) => {
    const s = document.createElement("script");
    s.src = HLS_JS;
    s.async = true;
    s.onload = () => resolve(w.Hls ?? null);
    s.onerror = () => resolve(null);
    document.head.appendChild(s);
  });
}

/** Plays the stream, and waits for it (polling) while it is off air. */
export function LivePlayer({ src, live }: { src: string; live: boolean }) {
  const video = useRef<HTMLVideoElement>(null);
  const [state, setState] = useState<"waiting" | "playing" | "error">(live ? "playing" : "waiting");

  useEffect(() => {
    let stop = false;
    let hls: { destroy(): void } | null = null;
    let timer: ReturnType<typeof setTimeout> | null = null;

    const start = async () => {
      const v = video.current;
      if (!v || stop) return;
      // Off air answers 404; ask again shortly rather than showing an error.
      const head = await fetch(src, { cache: "no-store" }).catch(() => null);
      if (stop) return;
      if (!head || !head.ok) {
        setState("waiting");
        timer = setTimeout(start, 5_000);
        return;
      }
      setState("playing");
      if (v.canPlayType("application/vnd.apple.mpegurl")) {
        v.src = src;
        return;
      }
      const Hls = await loadHls();
      if (stop) return;
      if (!Hls || !Hls.isSupported()) {
        setState("error");
        return;
      }
      const h = new Hls({ liveSyncDurationCount: 3 });
      hls = h;
      h.on(Hls.Events.ERROR, (_e, d) => {
        if (!d.fatal || stop) return;
        h.destroy();
        hls = null;
        setState("waiting");
        timer = setTimeout(start, 5_000);
      });
      h.loadSource(src);
      h.attachMedia(v);
    };
    void start();
    return () => {
      stop = true;
      if (timer) clearTimeout(timer);
      hls?.destroy();
    };
  }, [src]);

  return (
    <div>
      <video ref={video} controls autoPlay playsInline muted className="w-full aspect-video bg-black rounded" />
      <p className="mt-2 text-[15px] text-[var(--color-dim)]" role="status">
        {state === "waiting" ? "Off air. This page starts playing when the stream goes live." : state === "error" ? "This browser cannot play the stream here. Open the playlist link below in VLC or mpv." : "Live. Unmute to hear it."}
      </p>
    </div>
  );
}
