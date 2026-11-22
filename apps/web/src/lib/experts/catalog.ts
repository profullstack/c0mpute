// The Experts catalog: the signed manifests in ./manifests, admitted through
// the publisher allowlist. Only first-party publishers are accepted today;
// third-party publishing is switched on by adding publishers here (or via
// C0MPUTE_EXPERTS_PUBLISHERS), not by changing the format.

import automationEngineer from "./manifests/automation-engineer.json";
import legalInfo from "./manifests/legal-info.json";
import salesCoach from "./manifests/sales-coach.json";
import securityAnalyst from "./manifests/security-analyst.json";
import taxInfo from "./manifests/tax-info.json";
import { admitManifest, validateManifest, verifySignature, type ExpertManifest, type ModerationHook, type PublisherAllowlist } from "./manifest";

/** Publisher id → accepted ed25519 keys. Profullstack's key signs every first-party expert. */
export const FIRST_PARTY_PUBLISHERS: PublisherAllowlist = new Map([["profullstack", ["ed25519:0xbJbY50-amlN6iMvvDBpZ3N-txwRVofz6YSCCUs4PM"]]]);

/**
 * The allowlist in force: first-party, plus C0MPUTE_EXPERTS_PUBLISHERS
 * ("id=ed25519:key,id2=ed25519:key2") when the store is opened up.
 */
export function publisherAllowlist(env: Record<string, string | undefined> = process.env): PublisherAllowlist {
  const out = new Map<string, string[]>([...FIRST_PARTY_PUBLISHERS].map(([k, v]) => [k, [...v]]));
  for (const part of (env.C0MPUTE_EXPERTS_PUBLISHERS ?? "").split(",")) {
    const [id, key] = part.split("=").map((s) => s?.trim());
    if (!id || !key) continue;
    out.set(id, [...(out.get(id) ?? []), key]);
  }
  return out;
}

/** Display order on /experts. */
export const BUNDLED_MANIFESTS: unknown[] = [securityAnalyst, salesCoach, automationEngineer, legalInfo, taxInfo];

/** Synchronous check for the bundled set (signature + allowlist + shape), used on every request. */
function admittedSync(m: unknown, allow: PublisherAllowlist): m is ExpertManifest {
  if (validateManifest(m).length || !verifySignature(m)) return false;
  const x = m as ExpertManifest;
  return !!allow.get(x.publisher.id)?.includes(x.publisher.key);
}

let cache: { key: string; list: ExpertManifest[] } | null = null;
/**
 * Moderation refusals from the background labeler pass (slug → reason). On
 * globalThis: Next bundles instrumentation (which runs the pass) and the
 * routes as separate module graphs, so a module-level Map is not shared.
 */
const refused: Map<string, string> = ((globalThis as { __c0ExpertsRefused?: Map<string, string> }).__c0ExpertsRefused ??= new Map());

/** The admitted experts, in display order. A manifest that fails verification is left out and logged once. */
export function experts(env: Record<string, string | undefined> = process.env): ExpertManifest[] {
  const key = env.C0MPUTE_EXPERTS_PUBLISHERS ?? "";
  if (!cache || cache.key !== key) {
    const allow = publisherAllowlist(env);
    const list: ExpertManifest[] = [];
    for (const m of BUNDLED_MANIFESTS) {
      if (admittedSync(m, allow)) list.push(m);
      else console.error(`experts: refused manifest ${(m as { id?: string })?.id ?? "?"} (invalid, unsigned or publisher not allowed)`);
    }
    cache = { key, list };
  }
  return cache.list.filter((m) => !refused.has(m.slug));
}

export function expertBySlug(slug: string, env?: Record<string, string | undefined>): ExpertManifest | null {
  return experts(env).find((m) => m.slug === slug) ?? null;
}

/**
 * Run every admitted manifest past the DIP-0036 labeler once (at boot). A
 * refusal hides that expert until the next process; the labeler being down
 * changes nothing (fails open).
 */
export async function moderateCatalog(moderate: ModerationHook, env?: Record<string, string | undefined>): Promise<void> {
  const allow = publisherAllowlist(env);
  for (const m of experts(env)) {
    const r = await admitManifest(m, allow, moderate);
    if (!r.ok) {
      refused.set(m.slug, r.reason);
      console.error(`experts: ${m.slug} hidden: ${r.reason}`);
    }
  }
}

/** The public view of a manifest (everything: it is signed, public data), plus the store's effective price. */
export function publicManifest(m: ExpertManifest, priceUsd: number) {
  return { ...m, store: { price_usd: priceUsd, chat_model: `expert:${m.slug}`, url: `https://c0mpute.com/experts/${m.slug}` } };
}

/** Tests: reset caches. */
export function _resetCatalog() {
  cache = null;
  refused.clear();
}
