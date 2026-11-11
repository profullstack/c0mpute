// Client side of conversation sync (pure, shared by the web chat; the TUI and
// CLI follow the same rules). Last writer wins on `updated`; tombstones drop
// the local copy unless it changed after the delete.

export type SyncMsg = { role: string; content: string } & Record<string, unknown>;
export type SyncConvo = { id: string; title: string; model: string; messages: SyncMsg[]; updated: number };
export type ServerSummary = { id: string; title: string; model: string; updated: number; deleted: boolean };

/** Local-only or heavy fields never uploaded: media blobs, UI state, the live run id. */
const LOCAL_FIELDS = new Set(["image", "audio", "audioType", "revealed", "run"]);
const ROLES = new Set(["user", "assistant", "system", "note"]);

/** What goes to the server: no data: URLs, no local UI state. */
export function slimForSync<C extends SyncConvo>(c: C): SyncConvo {
  return {
    id: c.id,
    title: c.title,
    model: c.model,
    updated: c.updated,
    messages: c.messages
      .filter((m) => ROLES.has(m.role))
      .map((m) => {
        const out: SyncMsg = { role: m.role, content: m.content };
        for (const [k, v] of Object.entries(m)) {
          if (k === "role" || k === "content" || LOCAL_FIELDS.has(k)) continue;
          if (typeof v === "string" && v.startsWith("data:")) continue;
          out[k] = v;
        }
        return out;
      }),
  };
}

/** Take the server copy, keeping this device's media for messages that did not change. */
export function adopt<C extends SyncConvo>(local: C | undefined, server: SyncConvo): C {
  const messages = server.messages.map((m, i) => {
    const l = local?.messages[i];
    if (!l || l.role !== m.role || l.content !== m.content) return m;
    const keep: Record<string, unknown> = {};
    for (const k of ["image", "audio", "audioType", "revealed"]) if (l[k] !== undefined) keep[k] = l[k];
    return { ...m, ...keep };
  });
  return { ...(local ?? {}), ...server, messages } as C;
}

/** What one sync pass does, from the local list and the server's (since=1: every row, tombstones included). */
export function syncPlan(local: { id: string; updated: number }[], server: ServerSummary[]): { pull: string[]; push: string[]; drop: string[] } {
  const byId = new Map(server.map((s) => [s.id, s]));
  const mine = new Map(local.map((c) => [c.id, c]));
  const pull: string[] = [];
  const push: string[] = [];
  const drop: string[] = [];
  for (const s of server) {
    const l = mine.get(s.id);
    if (s.deleted) {
      if (l && l.updated <= s.updated) drop.push(s.id);
      else if (l) push.push(s.id);
      continue;
    }
    if (!l || s.updated > l.updated) pull.push(s.id);
    else if (s.updated < l.updated) push.push(s.id);
  }
  for (const l of local) if (!byId.has(l.id)) push.push(l.id);
  return { pull, push, drop };
}
