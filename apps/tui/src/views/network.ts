/**
 * Network: what c0mpute.com sees right now. Workers online, how many serve
 * each role, jobs in flight, the public node board, per-model prices, and
 * this account's balance.
 */
import type { Container, Theme } from "@profullstack/hqtui";
import { ago, gib, priceLabel, roleCounts, usd, type Loadable } from "../network.ts";
import type { AppState } from "../state.ts";
import { clock, panel, wrapBlock } from "./common.ts";

export interface NetworkHandlers {
  onScroll?: (delta: number) => void;
}

function stamp<T>(l: Loadable<T>): string {
  if (l.loading && !l.at) return "loading…";
  if (l.error && l.at) return `stale since ${clock(new Date(l.at))}: ${l.error}`;
  if (l.error) return l.error;
  return l.at ? `as of ${clock(new Date(l.at))}` : "";
}

/** "storage 9 · tts 10 · …" as wrapped lines (pure; tested). */
export function roleSummary(state: AppState, width: number): string[] {
  const counts = roleCounts(state.net.status.data);
  if (!counts.length) return [];
  return wrapBlock(counts.map(([r, n]) => `${r} ${n}`).join(" · "), width, "");
}

export function renderNetwork(ui: Container, state: AppState, theme: Theme, h: NetworkHandlers = {}): void {
  const { status, nodes, pricing } = state.net;
  const net = status.data?.network;
  const me = state.me.data;
  ui.column({ size: "fill" }, (col) => {
    panel(col, theme, "Network", { size: 7, subtitle: stamp(status) }, (p) => {
      p.row({ size: 1, gap: 2 }, (row) => {
        row.text(net ? `${net.workers_online} workers online` : status.error ? "offline" : "…", { fg: net ? theme.success : theme.warning, bold: true, size: 18 });
        row.text(net ? `${net.jobs_in_flight ?? 0} in flight · ${net.jobs_completed_24h ?? 0} done 24h` : "", { fg: theme.foreground, size: "fill" });
        row.text(
          me?.balance ? `balance ${usd(me.balance.spendable_usd)} · earned ${usd(me.balance.earned_spendable_usd)}` : state.creds.key ? "balance: …" : "balance: sign in",
          { fg: me?.balance ? theme.accent : theme.muted, size: 30, align: "right" },
        );
      });
      p.draw((surface) => {
        const lines = roleSummary(state, surface.width);
        if (!lines.length) surface.text(0, 0, status.error ? `${status.error} · r retries` : "loading roles…", { fg: theme.muted });
        lines.slice(0, surface.height).forEach((l, i) => surface.text(0, i, l, { fg: theme.muted }));
      });
    });
    if (state.netPanel === "nodes") {
      const rows = (nodes.data?.nodes ?? [])
        .slice()
        .sort((a, b) => a.last_seen_s - b.last_seen_s)
        .map((n) => ({
          id: n.id,
          name: n.name ?? "",
          class: n.class,
          cores: n.cores ?? "-",
          ram: gib(n.ram_bytes),
          bench: n.bench_score?.toLocaleString("en-US") ?? "-",
          version: n.version ?? "-",
          price: priceLabel(n.pricing),
          seen: ago(n.last_seen_s),
        }));
      panel(col, theme, `Nodes (${rows.length})`, { subtitle: `${stamp(nodes)} · v: prices` }, (p) => {
        if (!rows.length) {
          p.label(nodes.error ?? "loading the node board…", { fg: nodes.error ? theme.warning : theme.muted });
          return;
        }
        p.table({
          rows,
          columns: [
            { key: "id", title: "node", width: 8 },
            { key: "name", title: "operator", width: 12 },
            { key: "class", title: "class", width: 7 },
            { key: "cores", title: "cpu", width: 4, align: "right" },
            { key: "ram", title: "ram", width: 5, align: "right" },
            { key: "bench", title: "bench", width: 7, align: "right" },
            { key: "version", title: "ver", width: 6 },
            { key: "price", title: "price", width: "1fr" },
            { key: "seen", title: "seen", width: 4, align: "right" },
          ],
          offset: state.netScroll,
          scrollbar: true,
          zebra: true,
          onScroll: (d) => h.onScroll?.(d * 3),
          size: "fill",
        });
      });
    } else {
      const models = (pricing.data?.models ?? []).slice().sort((a, b) => Number(b.live ?? false) - Number(a.live ?? false) || a.model.localeCompare(b.model));
      const rows = models.map((m) => ({
        model: m.model,
        live: m.live ? "live" : "-",
        input: `$${m.input_per_m_usd}`,
        output: `$${m.output_per_m_usd}`,
        market: m.market ? `${m.market.reason ?? ""}${m.market.multiplier && m.market.multiplier !== 1 ? ` x${m.market.multiplier}` : ""}${m.market.providers !== undefined ? ` · ${m.market.providers} providers` : ""}` : (m.source ?? ""),
      }));
      panel(col, theme, `Prices per 1M tokens (${rows.length})`, { subtitle: `${stamp(pricing)} · v: nodes` }, (p) => {
        if (!rows.length) {
          p.label(pricing.error ?? "loading prices…", { fg: pricing.error ? theme.warning : theme.muted });
          return;
        }
        p.table({
          rows,
          columns: [
            { key: "model", title: "model", width: "1fr" },
            { key: "live", title: "", width: 4 },
            { key: "input", title: "in", width: 8, align: "right" },
            { key: "output", title: "out", width: 8, align: "right" },
            { key: "market", title: "market", width: 28 },
          ],
          offset: state.netScroll,
          scrollbar: true,
          zebra: true,
          rowColor: (r) => (r.live === "live" ? theme.success : theme.muted),
          onScroll: (d) => h.onScroll?.(d * 3),
          size: "fill",
        });
      });
    }
  });
}
