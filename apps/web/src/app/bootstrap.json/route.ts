/**
 * Serves the libp2p bootstrap seed list at https://c0mpute.com/bootstrap.json
 * (DIP-0010). Every c0mpute node fetches this on startup to find at least one
 * already-known peer and join the Kad-DHT.
 *
 * The peer list comes from the `BOOTSTRAP_PEERS_JSON` env var — a JSON array of
 * `{ id, addrs, operator?, region? }` objects — so seeds can be added, rotated,
 * or removed by editing an env var, with no code deploy (per DIP-0010: "so we
 * can update it without a binary release"). Unset/malformed → an empty list,
 * which is a valid file: nodes then fall back to their hardcoded list + mDNS
 * rather than seeing a 404.
 *
 * Shape mirrors `node/crates/c0mpute-net/src/bootstrap.rs::BootstrapFile`.
 */

import { NextResponse } from "next/server";

// Read the env var per-request so seed changes take effect without a rebuild.
export const dynamic = "force-dynamic";

const PROTOCOL_ID = "/c0mpute/kad/1.0.0";

interface BootstrapPeer {
  id: string;
  addrs: string[];
  operator?: string;
  region?: string;
}

function loadPeers(): BootstrapPeer[] {
  const raw = process.env.BOOTSTRAP_PEERS_JSON;
  if (!raw) return [];
  try {
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    // Keep only entries that carry an id and at least one address; a bad
    // entry must not poison the whole list.
    return parsed.filter(
      (p): p is BootstrapPeer =>
        p &&
        typeof p.id === "string" &&
        Array.isArray(p.addrs) &&
        p.addrs.length > 0,
    );
  } catch {
    // A malformed env var must never 500 the discovery mechanism for the
    // entire network — degrade to an empty (but valid) list.
    return [];
  }
}

export async function GET() {
  return NextResponse.json(
    { version: 1, protocol_id: PROTOCOL_ID, peers: loadPeers() },
    { headers: { "cache-control": "public, max-age=60, s-maxage=60" } },
  );
}
