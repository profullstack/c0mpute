import { describe, expect, test } from "bun:test";
import { LineSplitter, parseProgressLine, parsePublishSummary } from "../src/main/progress";

describe("publish progress", () => {
  test("splits a tty-style stream on CR and strips the redraw escape", () => {
    const s = new LineSplitter();
    expect(s.push("  hashing     [######..................]   1.00/4.00 GiB  300 MB/s\r\x1b[2K  hash")).toEqual(["hashing     [######..................]   1.00/4.00 GiB  300 MB/s"]);
    expect(s.push("ing     [####################....]   3.50/4.00 GiB  310 MB/s\r\x1b[2K")).toEqual([
      "hashing     [####################....]   3.50/4.00 GiB  310 MB/s",
    ]);
    expect(s.push("partial")).toEqual([]);
    expect(s.flush()).toEqual(["partial"]);
  });

  test("hashing is the first 30%", () => {
    const p = parseProgressLine("  hashing     [############............]   2.00/4.00 GiB  300 MB/s")!;
    expect(p.phase).toBe("hashing");
    expect(p.fraction).toBeCloseTo(0.5);
    expect(p.overall).toBeCloseTo(0.15);
  });

  test("replicating line from a striped site", () => {
    const p = parseProgressLine(
      "  replicating [#########...............]  37.5%  1.50/4.00 GiB of 3 copies, groups 2/9 complete, 1 without a copy, 12.3 MB/s",
    )!;
    expect(p.phase).toBe("replicating");
    expect(p.fraction).toBeCloseTo(0.375);
    expect(p.target).toBe(3);
    expect(p.overall).toBeCloseTo(0.3 + 0.375 * 0.7);
  });

  test("network copies on small sites", () => {
    expect(parseProgressLine("  2/3 network copies")).toMatchObject({ phase: "copies", copies: 2, target: 3 });
    expect(parseProgressLine("  1/3 network copies (1/3 preferred)")).toMatchObject({ copies: 1, target: 3 });
  });

  test("errors and other lines", () => {
    expect(parseProgressLine("Error: no such directory")?.phase).toBe("failed");
    expect(parseProgressLine("marketplace: 3 preferred host(s) at $0.01/GiB-month:")?.phase).toBe("info");
    expect(parseProgressLine("   ")).toBeNull();
    // tracing output on stderr is not progress
    expect(parseProgressLine("2026-11-11T00:12:45.199994Z DEBUG c0mpute_net::swarm: connection established peer_id=12D3")).toBeNull();
    expect(parseProgressLine("\x1b[2m2026-11-11T00:03:44.208281Z\x1b[0m \x1b[32m INFO\x1b[0m c0mpute_core: tracing initialised")).toBeNull();
  });

  test("reads the summary the CLI prints on stdout", () => {
    const out =
      "site:    ec601e419ec508ad0d9b\nversion: 1762800000000\ncopies:  3/3\npaid:    3 preferred host(s)\nurl:     https://gateway.c0mpute.com/s/ec601e419ec508ad0d9b/\n";
    expect(parsePublishSummary(out)).toEqual({ site: "ec601e419ec508ad0d9b", version: 1762800000000, copies: 3, url: "https://gateway.c0mpute.com/s/ec601e419ec508ad0d9b/" });
  });
});
