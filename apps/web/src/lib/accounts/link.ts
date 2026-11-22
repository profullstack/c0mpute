// `c0mpute node link`: a node proves it holds its identity key and joins an
// account, either for a signed-in operator (an API key from `c0mpute login`)
// or headlessly with a fleet enrollment token (`--enroll c0e_...`).
//
//   1. POST /api/v1/nodes/link/challenge {peer_id}       -> {challenge_id, message}
//   2. the node signs `message` with its libp2p ed25519 identity key
//   3. POST /api/v1/nodes/link {challenge_id, peer_id, signature, info?}
//
// The challenge is one-shot, bound to the account and the peer id, and expires
// in five minutes; the message names the site, account, node and challenge, so
// a signature cannot be replayed for another account, node, attempt or site.

import { randomBytes } from "node:crypto";
import type { EarningsStore } from "../earnings/store";
import { nodeKey } from "../earnings/store";
import { linkMessage, looksLikePeerId, verifyPeerSignature } from "./peer";
import type { AccountStore } from "./store";

/** Who is linking: a signed-in account, or an enrollment token's account. */
export type Linker =
  | { kind: "user"; userId: string; via: "key" | "web" }
  | { kind: "enroll"; userId: string; tokenHash: string; tokenName: string; tags: string[] };

export class LinkError extends Error {
  constructor(
    message: string,
    public status = 400,
  ) {
    super(message);
  }
}

/** What a node may say about itself when it links (shown in the fleet view, never trusted for money). */
export function cleanInfo(raw: unknown): Record<string, unknown> | null {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  const r = raw as Record<string, unknown>;
  const out: Record<string, unknown> = {};
  const str = (k: string, max = 120) => {
    if (typeof r[k] === "string" && r[k]) out[k] = (r[k] as string).slice(0, max);
  };
  const num = (k: string) => {
    if (typeof r[k] === "number" && Number.isFinite(r[k]) && (r[k] as number) >= 0) out[k] = r[k];
  };
  str("hostname", 64);
  str("version", 32);
  str("os", 64);
  str("arch", 16);
  str("gpu", 120);
  str("name", 64);
  num("cores");
  num("ram_bytes");
  num("disk_bytes");
  num("vram_bytes");
  return Object.keys(out).length ? out : null;
}

export function createLinkService(deps: { accounts: AccountStore; earnings: EarningsStore; site: string }) {
  const { accounts, earnings, site } = deps;

  return {
    async challenge(linker: Linker, peerId: unknown) {
      if (!looksLikePeerId(peerId)) throw new LinkError("peer_id: the node's peer id (c0mpute status --json | jq -r .peer_id)");
      const challenge = randomBytes(24).toString("base64url");
      const subject = JSON.stringify({ peer: peerId, via: linker.kind === "enroll" ? "token" : linker.via, token: linker.kind === "enroll" ? linker.tokenHash : null });
      const id = await accounts.putChallenge("node-link", challenge, { userId: linker.userId, subject });
      return { challenge_id: id, message: linkMessage({ site, accountId: linker.userId, peerId, challenge }), expires_in: 300 };
    },

    async complete(linker: Linker, body: { challenge_id?: unknown; peer_id?: unknown; signature?: unknown; info?: unknown; label?: unknown; tags?: string[] }) {
      if (!looksLikePeerId(body.peer_id)) throw new LinkError("peer_id is required");
      if (typeof body.signature !== "string") throw new LinkError("signature is required");
      const c = await accounts.takeChallenge("node-link", String(body.challenge_id ?? ""));
      if (!c) throw new LinkError("unknown, expired or already used challenge: run the link again", 410);
      let subject: { peer?: string; via?: string; token?: string | null } = {};
      try {
        subject = JSON.parse(c.subject ?? "{}");
      } catch {}
      // The challenge was issued to this account for this node, through this token.
      if (c.user_id !== linker.userId || subject.peer !== body.peer_id) throw new LinkError("this challenge was issued for another account or node", 403);
      if (linker.kind === "enroll" ? subject.token !== linker.tokenHash : subject.token) throw new LinkError("this challenge was issued through another credential", 403);
      const message = linkMessage({ site, accountId: linker.userId, peerId: body.peer_id, challenge: c.challenge });
      if (!verifyPeerSignature(body.peer_id, message, body.signature)) throw new LinkError("signature does not match the node's identity key", 401);

      const k = nodeKey(body.peer_id)!;
      const info = cleanInfo(body.info);
      const label = typeof body.label === "string" && body.label.trim() ? body.label.trim().slice(0, 64) : ((info?.name as string) ?? (info?.hostname as string) ?? null);
      const claim = await earnings.claim(linker.userId, { node: k.node, peer_id: body.peer_id, label, via: "signed" });
      const tags = [...(linker.kind === "enroll" ? linker.tags : []), ...(body.tags ?? [])];
      const movedFrom = await accounts.verifyNode(linker.userId, { node: k.node, peer_id: body.peer_id, via: linker.kind === "enroll" ? "token" : linker.via, tags, info });
      // A node verified to this account is no longer counted on the account it left.
      for (const other of movedFrom) await earnings.unclaim(other, k.node).catch(() => {});
      if (linker.kind === "enroll") await accounts.countEnrollUse(linker.tokenHash);
      await accounts.audit(linker.userId, {
        node: k.node,
        action: "link",
        args: { peer_id: body.peer_id, via: linker.kind === "enroll" ? `token:${linker.tokenName}` : linker.via },
        actor: linker.kind === "enroll" ? `token:${linker.tokenName}` : linker.via,
        outcome: movedFrom.length ? `linked (moved from ${movedFrom.length} other account)` : "linked",
      });
      return { node: claim, account_id: linker.userId, verified: true };
    },
  };
}
