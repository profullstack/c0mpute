// DIP-0036 operator policy for the c0mpute cloud node.
//
// c0mpute.com operates the free cloud node, so it is an operator under
// DIP-0036 L1 and chooses labelers like any other. Before a push to the cloud
// node is queued, its files go to the configured labeler (ThreatCrush by
// default); a file carrying a label this operator refuses blocks the push with
// 451 and the labeler's reason and appeal address.
//
// Trust lives here, not in the labeler's answer:
//  - the labeler's ed25519 key is pinned (env can replace it, never "learn" it)
//  - every label must verify, come from the pinned DID, be positive, unexpired,
//    and name the SHA-256 of a file *we sent*: a valid label for some other
//    file cannot be replayed onto this push
// The labeler being down never blocks a push (DIP-0036 "content fails open").
// Pushes to people's own nodes are their operator's call and are not checked.

import { createHash, createPublicKey, verify } from "node:crypto";
import type { PushFile } from "./push-shared";

export type LabelValue = "malware" | "phishing" | "cryptominer" | "spam";

export type Label = {
  kind: "label";
  ver: number;
  src: string;
  uri: string;
  val: string;
  neg: boolean;
  cts: string;
  exp: string;
  evidence?: string;
  sig: string;
};

export type LabelerConfig = {
  /** Base URL, e.g. https://threatcrush.com */
  url: string;
  /** DID labels must carry in `src`. */
  did: string;
  /** Raw ed25519 public key, base64url (32 bytes). */
  publicKey: string;
  /** Bearer token for /api/v1/labels/scan. */
  token: string;
  refuse: ReadonlySet<string>;
  appeal: string;
  timeoutMs: number;
};

/** ThreatCrush, pinned. https://threatcrush.com/api/v1/labels/manifest */
export const THREATCRUSH = {
  url: "https://threatcrush.com",
  did: "did:web:threatcrush.com",
  publicKey: "IbytqclmwUoFutZJHl526ZFWmg_TAVf7K9LNYuYGr4M",
  appeal: "support@threatcrush.com",
} as const;

export const DEFAULT_REFUSE = ["malware", "phishing", "cryptominer"] as const;

/** Read at request time (Next inlines literal process.env.NAME at build). */
export function labelerFromEnv(env: Record<string, string | undefined> = process.env): LabelerConfig | null {
  const get = (k: string) => (env[k] ?? "").trim();
  if (get("C0MPUTE_MODERATION") === "off") return null;
  const token = get("C0MPUTE_LABELER_TOKEN");
  if (!token) return null; // no scan token: moderation not configured, pushes pass (logged by the caller)
  const refuse = (get("C0MPUTE_MODERATION_REFUSE") || DEFAULT_REFUSE.join(","))
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  return {
    url: (get("C0MPUTE_LABELER_URL") || THREATCRUSH.url).replace(/\/+$/, ""),
    did: get("C0MPUTE_LABELER_DID") || THREATCRUSH.did,
    publicKey: get("C0MPUTE_LABELER_PUBLIC_KEY") || THREATCRUSH.publicKey,
    token,
    refuse: new Set(refuse),
    appeal: get("C0MPUTE_LABELER_APPEAL") || THREATCRUSH.appeal,
    timeoutMs: Number(get("C0MPUTE_LABELER_TIMEOUT_MS")) || 20_000,
  };
}

const SAFE = /^[\x21-\x7e]+$/;
const TS = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/;
const SPKI_ED25519 = Buffer.from("302a300506032b6570032100", "hex");

/** The exact bytes the labeler signs (DIP-0036 label, v1 framing). Null when a field is unsafe. */
export function canonicalLabel(l: Pick<Label, "src" | "uri" | "val" | "neg" | "cts" | "exp">): string | null {
  for (const v of [l.src, l.uri, l.val]) if (typeof v !== "string" || !v || v.length > 600 || !SAFE.test(v)) return null;
  if (typeof l.cts !== "string" || typeof l.exp !== "string" || !TS.test(l.cts) || !TS.test(l.exp)) return null;
  if (typeof l.neg !== "boolean") return null;
  return ["c0mpute-label-v1", l.src, l.uri, l.val, l.neg ? "1" : "0", l.cts, l.exp].join("\n");
}

export function verifyLabel(l: Label, publicKey: string): boolean {
  try {
    const msg = canonicalLabel(l);
    if (!msg || typeof l.sig !== "string") return false;
    const raw = Buffer.from(publicKey, "base64url");
    const sig = Buffer.from(l.sig, "base64url");
    if (raw.length !== 32 || sig.length !== 64) return false;
    const key = createPublicKey({ key: Buffer.concat([SPKI_ED25519, raw]), format: "der", type: "spki" });
    return verify(null, Buffer.from(msg, "utf8"), key, sig);
  } catch {
    return false;
  }
}

export type Verdict =
  | { verdict: "ok"; checked: number }
  | { verdict: "refused"; labels: Array<{ path: string; val: string; evidence?: string }>; message: string }
  | { verdict: "unchecked"; reason: string };

const sha256 = (s: string) => createHash("sha256").update(s, "utf8").digest("hex");

/** Labels that count: verified, from the pinned DID, positive, unexpired, refused here, about a file we sent. */
export function acceptedLabels(labels: unknown, cfg: Pick<LabelerConfig, "did" | "publicKey" | "refuse">, sent: Map<string, string>, now = Date.now()): Array<{ path: string; val: string; evidence?: string }> {
  if (!Array.isArray(labels)) return [];
  const out: Array<{ path: string; val: string; evidence?: string }> = [];
  for (const l of labels as Label[]) {
    if (!l || typeof l !== "object" || l.neg !== false || l.src !== cfg.did || !cfg.refuse.has(l.val)) continue;
    const path = typeof l.uri === "string" ? sent.get(l.uri) : undefined;
    if (!path) continue;
    const exp = Date.parse(l.exp);
    if (!Number.isFinite(exp) || exp <= now) continue;
    if (!verifyLabel(l, cfg.publicKey)) continue;
    if (!out.some((x) => x.path === path && x.val === l.val)) {
      out.push({ path, val: l.val, evidence: typeof l.evidence === "string" ? l.evidence.slice(0, 200) : undefined });
    }
  }
  return out;
}

/**
 * Ask the labeler about a push's inline files. Never throws: a broken labeler
 * returns "unchecked" and the caller lets the push through.
 */
export async function moderatePush(files: PushFile[], cfg: LabelerConfig, f: typeof fetch = fetch): Promise<Verdict> {
  const inline = files.filter((x): x is PushFile & { text: string } => typeof x.text === "string");
  if (inline.length === 0) return { verdict: "ok", checked: 0 };
  const sent = new Map<string, string>();
  for (const x of inline) sent.set(`sha256:${sha256(x.text)}`, x.path);
  let res: Response;
  try {
    res = await f(`${cfg.url}/api/v1/labels/scan`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${cfg.token}`, "user-agent": "c0mpute-cloud-node-moderation/1 (+https://c0mpute.com)" },
      body: JSON.stringify({ files: inline.map((x) => ({ path: x.path, content_b64: Buffer.from(x.text, "utf8").toString("base64") })) }),
      signal: AbortSignal.timeout(cfg.timeoutMs),
      redirect: "error",
    });
  } catch (e) {
    return { verdict: "unchecked", reason: `labeler unreachable: ${e instanceof Error ? e.name : "error"}` };
  }
  if (!res.ok) return { verdict: "unchecked", reason: `labeler answered ${res.status}` };
  let body: { labels?: unknown };
  try {
    body = (await res.json()) as { labels?: unknown };
  } catch {
    return { verdict: "unchecked", reason: "labeler answered non-JSON" };
  }
  const hits = acceptedLabels(body?.labels, cfg, sent);
  if (hits.length === 0) return { verdict: "ok", checked: inline.length };
  const list = hits.map((h) => `${h.path} (${h.val})`).join(", ");
  return {
    verdict: "refused",
    labels: hits,
    message: `refused by the c0mpute cloud node's content policy: ${list} flagged by ${cfg.did}. If this is wrong, appeal to ${cfg.appeal} with the file's SHA-256; see https://github.com/profullstack/c0mpute/blob/master/dips/v2.x/0036-layered-moderation.md`,
  };
}
