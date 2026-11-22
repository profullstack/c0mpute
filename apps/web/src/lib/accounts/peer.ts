// Proof that a request comes from the holder of a c0mpute node's identity key.
//
// A node's peer id (`12D3KooW...`) is a libp2p identity multihash that embeds
// its ed25519 public key, so a signature can be checked against the peer id
// alone: no key registry, no trust in what the node says about itself.

import { createPublicKey, verify } from "node:crypto";

const B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const SPKI_ED25519 = Buffer.from("302a300506032b6570032100", "hex");

export function base58Decode(s: string): Uint8Array | null {
  if (!s || s.length > 128) return null;
  let n = 0n;
  for (const ch of s) {
    const i = B58.indexOf(ch);
    if (i < 0) return null;
    n = n * 58n + BigInt(i);
  }
  const bytes: number[] = [];
  while (n > 0n) {
    bytes.unshift(Number(n & 0xffn));
    n >>= 8n;
  }
  for (const ch of s) {
    if (ch !== "1") break;
    bytes.unshift(0);
  }
  return Uint8Array.from(bytes);
}

/**
 * The raw 32-byte ed25519 public key inside a peer id, or null when the peer
 * id is not an ed25519 identity multihash (RSA/secp256k1 ids hash the key, so
 * nothing can be read back from them; c0mpute nodes are all ed25519).
 */
export function peerPublicKey(peerId: string): Buffer | null {
  const b = base58Decode(peerId);
  // identity multihash (0x00), length 36, protobuf PublicKey { Type = Ed25519 (1), Data = 32 bytes }
  if (!b || b.length !== 38 || b[0] !== 0x00 || b[1] !== 36) return null;
  if (b[2] !== 0x08 || b[3] !== 0x01 || b[4] !== 0x12 || b[5] !== 0x20) return null;
  return Buffer.from(b.slice(6));
}

export function looksLikePeerId(s: unknown): s is string {
  return typeof s === "string" && /^12D3KooW[1-9A-HJ-NP-Za-km-z]{44,46}$/.test(s) && peerPublicKey(s) !== null;
}

/** Does `signature` (base64 or base64url, 64 bytes) sign `message` with the key embedded in `peerId`? */
export function verifyPeerSignature(peerId: string, message: string, signature: string): boolean {
  const raw = peerPublicKey(peerId);
  if (!raw || typeof signature !== "string" || signature.length > 200) return false;
  let sig: Buffer;
  try {
    sig = Buffer.from(signature.replace(/-/g, "+").replace(/_/g, "/"), "base64");
  } catch {
    return false;
  }
  if (sig.length !== 64) return false;
  try {
    const key = createPublicKey({ key: Buffer.concat([SPKI_ED25519, raw]), format: "der", type: "spki" });
    return verify(null, Buffer.from(message, "utf8"), key, sig);
  } catch {
    return false;
  }
}

/**
 * The exact text a node signs to link itself to an account. Must match
 * `link_message` in the CLI (node/crates/c0mpute-cli/src/account_cli.rs).
 * Binding the account id, the peer id, the server's one-shot challenge and the
 * site stops a signature made for one account, node, attempt or site from
 * being replayed for another.
 */
export function linkMessage(o: { site: string; accountId: string; peerId: string; challenge: string }): string {
  return `c0mpute node link v1\nsite: ${o.site}\naccount: ${o.accountId}\nnode: ${o.peerId}\nchallenge: ${o.challenge}`;
}
