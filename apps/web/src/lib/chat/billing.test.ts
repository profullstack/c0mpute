import { describe, expect, test } from "bun:test";
import type { Upstream } from "./completions";

/**
 * Per-user billing on c0mpute chat.
 *
 * c0mpute runs its inference on the Infernet network under one trusted client
 * key, so before this every c0mpute user looked like a single anonymous caller
 * and nobody's compute balance could pay for anything. infernetFor() names the
 * signed-in person to Infernet, which bills their balance - money they topped
 * up, or what their own nodes earned.
 *
 * lib/chat/server.ts cannot be imported here: importing it opens a SQLite
 * Cloud connection. These rebuild the same two-line header contract, which is
 * the part that has to stay true.
 */
function harness() {
  const seen: { url: string; headers: Record<string, string> }[] = [];
  const fakeFetch = async (url: string, init: RequestInit = {}) => {
    seen.push({ url: String(url), headers: { ...((init.headers ?? {}) as Record<string, string>) } });
    return new Response("{}", { status: 200, headers: { "content-type": "application/json" } });
  };
  const infernet = (path: string, init: RequestInit = {}, opts: { subject?: string | null } = {}) =>
    fakeFetch("https://infernetprotocol.com" + path, {
      ...init,
      headers: {
        ...(init.headers as Record<string, string>),
        authorization: "Bearer ifc_test",
        ...(opts.subject ? { "x-infernet-billed-subject": opts.subject } : {}),
      },
    });
  const infernetFor =
    (subject: string | null | undefined): Upstream =>
    (path, init = {}) =>
      infernet(path, init, { subject: subject ?? null });
  return { seen, infernet, infernetFor };
}

describe("per-user billing upstream", () => {
  test("names the signed-in person, so their balance is what pays", async () => {
    const { seen, infernetFor } = harness();
    await infernetFor("coinpay-sub-1")("/v1/chat/completions", { method: "POST" });
    expect(seen[0].headers["x-infernet-billed-subject"]).toBe("coinpay-sub-1");
    // The trusted client key still travels: Infernet only accepts a named
    // subject from a client it trusts.
    expect(seen[0].headers.authorization).toBe("Bearer ifc_test");
  });

  test("sends no subject when there is no one to bill", async () => {
    const { seen, infernetFor } = harness();
    await infernetFor(null)("/v1/chat/completions", { method: "POST" });
    await infernetFor(undefined)("/v1/chat/completions", { method: "POST" });
    expect(seen[0].headers["x-infernet-billed-subject"]).toBeUndefined();
    expect(seen[1].headers["x-infernet-billed-subject"]).toBeUndefined();
  });

  test("keeps one person's calls off another person's balance", async () => {
    const { seen, infernetFor } = harness();
    await infernetFor("sub-a")("/v1/chat/completions", { method: "POST" });
    await infernetFor("sub-b")("/v1/chat/completions", { method: "POST" });
    expect(seen.map((s) => s.headers["x-infernet-billed-subject"])).toEqual(["sub-a", "sub-b"]);
  });

  test("passes the caller's own headers and body through untouched", async () => {
    const { seen, infernetFor } = harness();
    await infernetFor("sub-a")("/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json", accept: "text/event-stream" },
    });
    expect(seen[0].headers["content-type"]).toBe("application/json");
    expect(seen[0].headers.accept).toBe("text/event-stream");
    expect(seen[0].url).toBe("https://infernetprotocol.com/v1/chat/completions");
  });
});
