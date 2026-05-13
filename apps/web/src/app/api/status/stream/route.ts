/**
 * GET /api/status/stream — SSE endpoint for live status updates.
 *
 * Opens a persistent SSE connection. On the server side, polls the
 * status source (aggregator or stub) every 15s and pushes payloads to
 * the client. When the real aggregator ships with native push support,
 * the internal poll can be swapped for a persistent connection.
 *
 * The client (LiveBadge) subscribes to this stream and triggers
 * router.refresh() on each payload so the page re-renders with
 * fresh data.
 */

import { NextRequest } from "next/server";
import { getStatusPayload } from "@/lib/status";

const POLL_INTERVAL_MS = 15_000;

export async function GET(_request: NextRequest) {
  let aborted = false;

  const stream = new ReadableStream({
    async start(controller) {
      const push = async () => {
        if (aborted) return;
        try {
          const payload = await getStatusPayload();
          controller.enqueue(
            new TextEncoder().encode(`data: ${JSON.stringify(payload)}\n\n`),
          );
        } catch {
          // Silently skip failed polls — the client has a fallback timer.
        }
      };

      await push();

      const interval = setInterval(async () => {
        if (aborted) {
          clearInterval(interval);
          return;
        }
        await push();
      }, POLL_INTERVAL_MS);

      _request.signal.addEventListener("abort", () => {
        aborted = true;
        clearInterval(interval);
      });
    },
    cancel() {
      aborted = true;
    },
  });

  return new Response(stream, {
    headers: {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
      connection: "keep-alive",
    },
  });
}
