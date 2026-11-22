// Requests a node makes on its own behalf (fetch its fleet commands, report a
// result) are signed with its identity key over a timestamped message.

import { pollMessage } from "./fleet";
import { looksLikePeerId, verifyPeerSignature } from "./peer";

/** How far a node's clock may be off. */
export const SKEW_S = 300;

export function checkNodeRequest(
  site: string,
  body: { peer_id?: unknown; ts?: unknown; signature?: unknown; id?: unknown; ok?: unknown },
  kind: "poll" | "result",
  nowS = Math.floor(Date.now() / 1000),
): string | null {
  if (!looksLikePeerId(body.peer_id)) return "peer_id is required";
  const ts = Number(body.ts);
  if (!Number.isInteger(ts) || Math.abs(nowS - ts) > SKEW_S) return "ts: unix seconds within 5 minutes of now (check the node's clock)";
  if (typeof body.signature !== "string") return "signature is required";
  if (kind === "result" && (typeof body.id !== "string" || typeof body.ok !== "boolean")) return "id and ok are required";
  const msg = pollMessage({ site, peerId: body.peer_id, ts, kind, id: body.id as string, ok: body.ok as boolean });
  return verifyPeerSignature(body.peer_id, msg, body.signature) ? null : "signature does not match the node's identity key";
}
