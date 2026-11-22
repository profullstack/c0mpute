// Signed app manifests (format "c0mpute.manifest/v1"), the format the open
// p2p app store will use. An Expert is the first kind: a model choice, a
// system prompt, the MCP plugins it may call, pricing and a category, signed
// by its publisher. Spec: docs/experts.md and dips/0037-signed-app-manifests.md.
//
// Signing: ed25519 over the canonical JSON of the manifest without its
// `signature` field (keys sorted at every depth, no whitespace). The publisher
// key is "ed25519:<base64url raw 32 bytes>"; the signature names the same key.
// Acceptance is separate from validity: a manifest is admitted only when its
// signature verifies AND its publisher (id + key) is on the store's allowlist.
// Today the allowlist is first-party only; opening it to third parties is a
// config change, not a format change.

import { createPrivateKey, createPublicKey, sign as edSign, verify as edVerify, type KeyObject } from "node:crypto";

export const MANIFEST_FORMAT = "c0mpute.manifest/v1";
export const MANIFEST_KINDS = ["expert"] as const;
export type ManifestKind = (typeof MANIFEST_KINDS)[number];

export const EXPERT_CATEGORIES = ["security", "sales", "automation", "legal", "tax"] as const;
export type ExpertCategory = (typeof EXPERT_CATEGORIES)[number];

/** Categories the store never lists, whoever signs them (legal + safety hold). */
export const BLOCKED_CATEGORIES = ["therapy", "mental-health", "social-work", "medical", "health"] as const;

export type Publisher = {
  /** Stable publisher id (lowercase slug), e.g. "profullstack". */
  id: string;
  name: string;
  /** Optional DID for the publisher (did:web:..., did:key:...). */
  did?: string;
  /** "ed25519:<base64url raw public key>" */
  key: string;
};

export type McpTool = {
  /** Short id, e.g. "threatcrush". */
  id: string;
  /** Remote MCP server URL (https). The expert may use a chat plugin only when its URL host matches. */
  url: string;
  why?: string;
};

export type ExpertManifest = {
  format: typeof MANIFEST_FORMAT;
  kind: "expert";
  /** "<publisher.id>/<slug>" */
  id: string;
  slug: string;
  name: string;
  version: string;
  category: ExpertCategory;
  summary: string;
  description: string;
  publisher: Publisher;
  model: {
    /** Network models to try, in order (fail over on 5xx / no node). */
    prefer: string[];
    temperature?: number;
    max_tokens?: number;
  };
  system_prompt: string;
  skills: string[];
  tools: { mcp: McpTool[] };
  example_questions: string[];
  /** Shown with every answer when set (legal, tax). */
  disclaimer?: string | null;
  pricing: {
    model: "subscription";
    /** Publisher's list price per month in USD; the store's config may override it. */
    monthly_usd: number;
    trial_days: number;
  };
  created_at: string;
  signature?: ManifestSignature;
};

export type ManifestSignature = { alg: "ed25519"; key: string; sig: string };

// ── canonical JSON ───────────────────────────────────────────────────────────

/** JSON with object keys sorted at every depth and no whitespace. Undefined fields are dropped. */
export function canonicalJson(v: unknown): string {
  if (v === null || typeof v === "boolean" || typeof v === "string") return JSON.stringify(v);
  if (typeof v === "number") {
    if (!Number.isFinite(v)) throw new Error("canonicalJson: non-finite number");
    return JSON.stringify(v);
  }
  if (Array.isArray(v)) return `[${v.map((x) => (x === undefined ? "null" : canonicalJson(x))).join(",")}]`;
  if (typeof v === "object") {
    const o = v as Record<string, unknown>;
    const keys = Object.keys(o).filter((k) => o[k] !== undefined).sort();
    return `{${keys.map((k) => `${JSON.stringify(k)}:${canonicalJson(o[k])}`).join(",")}}`;
  }
  throw new Error(`canonicalJson: unsupported ${typeof v}`);
}

/** The bytes a signature covers: the manifest minus `signature`. */
export function signingBytes(m: object): Buffer {
  const { signature: _s, ...rest } = m as Record<string, unknown>;
  return Buffer.from(canonicalJson(rest), "utf8");
}

// ── keys ─────────────────────────────────────────────────────────────────────

const SPKI_ED25519 = Buffer.from("302a300506032b6570032100", "hex");
const PKCS8_ED25519 = Buffer.from("302e020100300506032b657004220420", "hex");

export function publicKeyObject(key: string): KeyObject | null {
  const m = /^ed25519:([A-Za-z0-9_-]{43})$/.exec(key);
  if (!m) return null;
  const raw = Buffer.from(m[1], "base64url");
  if (raw.length !== 32) return null;
  try {
    return createPublicKey({ key: Buffer.concat([SPKI_ED25519, raw]), format: "der", type: "spki" });
  } catch {
    return null;
  }
}

/** A private key from a 32-byte seed (base64url / base64 / hex) or a PEM. */
export function privateKeyObject(secret: string): KeyObject {
  const s = secret.trim();
  if (s.includes("BEGIN")) return createPrivateKey(s);
  let seed: Buffer;
  if (/^[0-9a-f]{64}$/i.test(s)) seed = Buffer.from(s, "hex");
  else seed = Buffer.from(s.replace(/^ed25519-secret:/, ""), s.includes("+") || s.includes("/") ? "base64" : "base64url");
  if (seed.length !== 32) throw new Error("signing key must be a 32-byte ed25519 seed");
  return createPrivateKey({ key: Buffer.concat([PKCS8_ED25519, seed]), format: "der", type: "pkcs8" });
}

/** "ed25519:<base64url>" for a private or public key. */
export function publicKeyString(k: KeyObject): string {
  const pub = k.type === "private" ? createPublicKey(k) : k;
  const der = pub.export({ format: "der", type: "spki" }) as Buffer;
  return `ed25519:${der.subarray(der.length - 32).toString("base64url")}`;
}

// ── sign / verify ────────────────────────────────────────────────────────────

export function signManifest<T extends object>(m: T, privateKey: KeyObject): T & { signature: ManifestSignature } {
  const key = publicKeyString(privateKey);
  const sig = edSign(null, signingBytes(m), privateKey).toString("base64url");
  return { ...(m as T), signature: { alg: "ed25519", key, sig } };
}

/** Does the signature verify, and is it by the key the manifest names as its publisher? */
export function verifySignature(m: unknown): boolean {
  if (!m || typeof m !== "object") return false;
  const x = m as { signature?: ManifestSignature; publisher?: Publisher };
  const s = x.signature;
  if (!s || s.alg !== "ed25519" || typeof s.sig !== "string" || typeof s.key !== "string") return false;
  if (!x.publisher || x.publisher.key !== s.key) return false;
  const pub = publicKeyObject(s.key);
  if (!pub) return false;
  try {
    return edVerify(null, signingBytes(m), pub, Buffer.from(s.sig, "base64url"));
  } catch {
    return false;
  }
}

// ── validation ───────────────────────────────────────────────────────────────

const SLUG = /^[a-z0-9][a-z0-9-]{1,47}$/;
const SEMVER = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;
const str = (v: unknown, max: number) => typeof v === "string" && v.trim().length > 0 && v.length <= max;

/** Structural checks; returns the problems (empty when the manifest is well-formed). */
export function validateManifest(m: unknown): string[] {
  const p: string[] = [];
  if (!m || typeof m !== "object" || Array.isArray(m)) return ["manifest must be an object"];
  const x = m as Partial<ExpertManifest> & Record<string, unknown>;
  if (x.format !== MANIFEST_FORMAT) p.push(`format must be "${MANIFEST_FORMAT}"`);
  if (x.kind !== "expert") p.push('kind must be "expert"');
  if (typeof x.slug !== "string" || !SLUG.test(x.slug)) p.push("slug must be 2-48 of a-z 0-9 -");
  const pub = x.publisher as Publisher | undefined;
  if (!pub || typeof pub !== "object") p.push("publisher is required");
  else {
    if (typeof pub.id !== "string" || !SLUG.test(pub.id)) p.push("publisher.id must be a slug");
    if (!str(pub.name, 80)) p.push("publisher.name is required");
    if (typeof pub.key !== "string" || !publicKeyObject(pub.key)) p.push("publisher.key must be ed25519:<base64url>");
  }
  if (pub && x.slug && x.id !== `${pub.id}/${x.slug}`) p.push("id must be <publisher.id>/<slug>");
  if (!str(x.name, 60)) p.push("name is required (<= 60 chars)");
  if (typeof x.version !== "string" || !SEMVER.test(x.version)) p.push("version must be semver");
  if (typeof x.category !== "string" || (BLOCKED_CATEGORIES as readonly string[]).includes(x.category)) p.push("category is not allowed in this store");
  else if (!(EXPERT_CATEGORIES as readonly string[]).includes(x.category)) p.push(`category must be one of ${EXPERT_CATEGORIES.join(", ")}`);
  if (!str(x.summary, 200)) p.push("summary is required (<= 200 chars)");
  if (!str(x.description, 2000)) p.push("description is required (<= 2000 chars)");
  if (!str(x.system_prompt, 12_000)) p.push("system_prompt is required (<= 12000 chars)");
  const model = x.model as ExpertManifest["model"] | undefined;
  if (!model || !Array.isArray(model.prefer) || model.prefer.length === 0 || model.prefer.length > 6 || !model.prefer.every((s) => str(s, 200)))
    p.push("model.prefer must list 1-6 model ids");
  if (model?.temperature !== undefined && !(typeof model.temperature === "number" && model.temperature >= 0 && model.temperature <= 2)) p.push("model.temperature must be 0-2");
  if (model?.max_tokens !== undefined && !(Number.isInteger(model.max_tokens) && model.max_tokens >= 1 && model.max_tokens <= 8192)) p.push("model.max_tokens must be 1-8192");
  if (!Array.isArray(x.skills) || !x.skills.every((s) => str(s, 80))) p.push("skills must be a list of short strings");
  const tools = x.tools as ExpertManifest["tools"] | undefined;
  if (!tools || !Array.isArray(tools.mcp)) p.push("tools.mcp must be a list");
  else
    for (const t of tools.mcp) {
      if (!t || typeof t.id !== "string" || !SLUG.test(t.id)) p.push("tools.mcp[].id must be a slug");
      try {
        if (new URL(t.url).protocol !== "https:") p.push("tools.mcp[].url must be https");
      } catch {
        p.push("tools.mcp[].url must be a URL");
      }
    }
  if (!Array.isArray(x.example_questions) || x.example_questions.length < 1 || !x.example_questions.every((s) => str(s, 300))) p.push("example_questions must list at least one question");
  if (x.disclaimer !== undefined && x.disclaimer !== null && !str(x.disclaimer, 600)) p.push("disclaimer must be a string (<= 600 chars) or null");
  if (x.category === "legal" || x.category === "tax") {
    if (!str(x.disclaimer, 600)) p.push(`${x.category} experts must carry a disclaimer`);
  }
  const pr = x.pricing as ExpertManifest["pricing"] | undefined;
  if (!pr || pr.model !== "subscription" || !(typeof pr.monthly_usd === "number" && pr.monthly_usd >= 0 && pr.monthly_usd <= 1000) || !(Number.isInteger(pr.trial_days) && pr.trial_days >= 0 && pr.trial_days <= 365))
    p.push("pricing must be { model: subscription, monthly_usd 0-1000, trial_days 0-365 }");
  if (typeof x.created_at !== "string" || Number.isNaN(Date.parse(x.created_at))) p.push("created_at must be an ISO date");
  return p;
}

// ── admission (signature + allowlist + optional moderation) ──────────────────

/** Publishers the store accepts: id → keys. First-party only today. */
export type PublisherAllowlist = ReadonlyMap<string, readonly string[]>;

export type ModerationHook = (m: ExpertManifest) => Promise<{ verdict: "ok" | "unchecked" | "refused"; message?: string }>;

export type Admission = { ok: true; manifest: ExpertManifest; moderation: "ok" | "unchecked" | "skipped" } | { ok: false; reason: string };

/**
 * Admit a manifest into the store: well-formed, signature verifies, publisher
 * id AND key on the allowlist, and (when a moderation hook is given) not
 * refused by the DIP-0036 labeler. The labeler being down never blocks a
 * first-party manifest ("content fails open"); a refusal always does.
 */
export async function admitManifest(m: unknown, allow: PublisherAllowlist, moderate?: ModerationHook): Promise<Admission> {
  const problems = validateManifest(m);
  if (problems.length) return { ok: false, reason: `invalid manifest: ${problems.join("; ")}` };
  if (!verifySignature(m)) return { ok: false, reason: "signature does not verify" };
  const x = m as ExpertManifest;
  const keys = allow.get(x.publisher.id);
  if (!keys) return { ok: false, reason: `publisher ${x.publisher.id} is not accepted by this store (first-party only)` };
  if (!keys.includes(x.publisher.key)) return { ok: false, reason: `key is not an accepted key for publisher ${x.publisher.id}` };
  if (!moderate) return { ok: true, manifest: x, moderation: "skipped" };
  const v = await moderate(x).catch(() => ({ verdict: "unchecked" as const }));
  if (v.verdict === "refused") return { ok: false, reason: v.message || "refused by the content labeler" };
  return { ok: true, manifest: x, moderation: v.verdict };
}
