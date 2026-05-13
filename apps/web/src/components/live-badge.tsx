"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";

export default function LiveBadge() {
  const router = useRouter();
  const [lastRefresh, setLastRefresh] = useState(() => Date.now());
  const [connected, setConnected] = useState(false);
  const [tickRender, setTickRender] = useState(0);

  useEffect(() => {
    let es: EventSource | null = null;
    let fallbackTimer: ReturnType<typeof setInterval> | null = null;
    let ticker: ReturnType<typeof setInterval> | null = null;

    const doRefresh = () => {
      if (
        typeof document !== "undefined" &&
        document.visibilityState === "hidden"
      )
        return;
      router.refresh();
      setLastRefresh(Date.now());
    };

    const onVisible = () => {
      if (document.visibilityState === "visible") doRefresh();
    };

    ticker = setInterval(() => setTickRender((n) => n + 1), 1000);
    document.addEventListener("visibilitychange", onVisible);

    try {
      es = new EventSource("/api/status/stream");
      es.addEventListener("open", () => setConnected(true));
      es.addEventListener("message", () => doRefresh());
      es.onerror = () => setConnected(false);
    } catch {
      setConnected(false);
    }

    fallbackTimer = setInterval(doRefresh, 60_000);

    return () => {
      if (es) {
        try {
          es.close();
        } catch {
          /* ignore */
        }
      }
      if (fallbackTimer) clearInterval(fallbackTimer);
      if (ticker) clearInterval(ticker);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [router]);

  const sinceMs = Date.now() - lastRefresh;
  const sinceLabel =
    sinceMs < 1000 ? "now" : `${Math.floor(sinceMs / 1000)}s ago`;
  const tone = connected
    ? "border-emerald-400/30 bg-emerald-400/10 text-emerald-200"
    : "border-[var(--color-warn)]/30 bg-[var(--color-warn)]/10 text-[var(--color-warn)]";
  const dotColor = connected ? "bg-emerald-400" : "bg-[var(--color-warn)]";

  return (
    <span
      className={`inline-flex items-center gap-2 rounded-full border px-3 py-1 text-xs font-mono ${tone}`}
    >
      <span className="relative flex h-2 w-2" aria-hidden="true">
        <span
          className={`absolute inline-flex h-full w-full animate-ping rounded-full opacity-75 ${dotColor}`}
        />
        <span
          className={`relative inline-flex h-2 w-2 rounded-full ${dotColor}`}
        />
      </span>
      <span>
        Live · {sinceLabel}
        <span className="hidden">{tickRender}</span>
      </span>
    </span>
  );
}
