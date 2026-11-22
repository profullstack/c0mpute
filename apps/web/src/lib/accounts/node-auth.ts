// Requests a node makes on its own behalf (fetch its fleet commands, report a
// result) are signed with its identity key over a timestamped message.

import { MAX_STATE_BYTES, pollMessage, sha256Hex } from "./fleet";
import { looksLikePeerId, verifyPeerSignature } from "./peer";

/** How far a node's clock may be off. */
export const SKEW_S = 300;

export function checkNodeRequest(
  site: string,
  body: { peer_id?: unknown; ts?: unknown; signature?: unknown; id?: unknown; ok?: unknown; state?: unknown },
  kind: "poll" | "result",
  nowS = Math.floor(Date.now() / 1000),
): string | null {
  if (!looksLikePeerId(body.peer_id)) return "peer_id is required";
  const ts = Number(body.ts);
  if (!Number.isInteger(ts) || Math.abs(nowS - ts) > SKEW_S) return "ts: unix seconds within 5 minutes of now (check the node's clock)";
  if (typeof body.signature !== "string") return "signature is required";
  if (kind === "result" && (typeof body.id !== "string" || typeof body.ok !== "boolean")) return "id and ok are required";
  // A poll may carry the node's state (a JSON string); its SHA-256 is in the signed text.
  let stateSha256: string | undefined;
  if (kind === "poll" && body.state !== undefined) {
    if (typeof body.state !== "string" || body.state.length > MAX_STATE_BYTES) return "state: a JSON string under 16 KB";
    stateSha256 = sha256Hex(body.state);
  }
  const msg = pollMessage({ site, peerId: body.peer_id, ts, kind, id: body.id as string, ok: body.ok as boolean, stateSha256 });
  return verifyPeerSignature(body.peer_id, msg, body.signature) ? null : "signature does not match the node's identity key";
}
