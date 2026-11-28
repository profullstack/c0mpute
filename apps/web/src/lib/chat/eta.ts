// A turn's status line, made live: the network says "running on a node
// (about 211 s)" once, and a static number reads like a hang. This turns the
// estimate into a countdown from when the status arrived, says plainly when
// the node runs past it, and adds how long the turn has waited so far.

/** 0:07, 3:31, 1:02:05 */
export function clock(totalS: number): string {
  const s = Math.max(0, Math.round(totalS));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, "0");
  return h ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
}

const ETA = /\(about (\d+(?:\.\d+)?) ?(s|sec|secs|seconds|min|mins|minutes)\)/i;

/**
 * @param status    the latest status line (or null before the first one)
 * @param statusAt  when that status arrived (ms)
 * @param startedAt when the turn was sent (ms)
 * @param now       the clock (ms)
 */
export function liveStatus(status: string | null | undefined, statusAt: number, startedAt: number, now: number): string {
  const waited = clock((now - startedAt) / 1000);
  if (!status) return `sending it to the network… · ${waited}`;
  const m = ETA.exec(status);
  if (!m) return `${status} · ${waited}`;
  const etaS = Number(m[1]) * (/^m/i.test(m[2]) ? 60 : 1);
  const left = etaS - (now - statusAt) / 1000;
  const live = left >= 0 ? `(about ${clock(left)} left)` : `(${clock(-left)} over the estimate)`;
  return `${status.replace(ETA, live)} · ${waited}`;
}
